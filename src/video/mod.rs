//! Video playback via FFmpeg's libraries (cargo feature `video`).
//!
//! One session thread demuxes the file in-process. Video packets decode on
//! the GPU's fixed-function block when the platform and codec allow,
//! otherwise multithreaded software, into planar YUV that flows through a
//! bounded channel for backpressure; the GPU converts it to RGB at display.
//! Audio packets decode and resample to f32 PCM consumed by a rodio sink on
//! its own thread. The sink's position is the master clock. Files without
//! audio fall back to a pause-aware wall clock. Seeking opens a fresh
//! session at the target (`avformat` seek before decode begins). No
//! external processes are involved anywhere.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;

mod audio;
mod frame;
mod hw;
use audio::{AudioCmd, send_audio_frame, spawn_audio_output};
use frame::copy_plane;
pub use frame::{VideoFrame, YuvFormat, YuvMatrix, YuvRange};
use hw::{download_frame, is_hw_frame, try_init_hw};

/// Video container extensions offered in the file list.
pub const EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

/// Whether this path looks like a video file.
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Initialize FFmpeg once, with routine decoder chatter silenced, because
/// libav's default callback prints warnings (e.g. mp3float's "could not
/// update timestamps for skipped samples") straight to the console.
/// Real failures still log at error level.
fn init_ffmpeg() -> Result<(), ffmpeg::Error> {
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    Ok(())
}

/// A decoded first frame in RGBA, with the source's native dimensions.
pub struct FirstFrame {
    pub width: u32,
    pub height: u32,
    pub native_size: (u32, u32),
    pub pixels: Vec<u8>,
}

/// Decode the first video frame of `path` as RGBA, optionally scaled to
/// fit `max_dim`. Blocking, run on a worker. Also serves AVIF stills,
/// which are AV1 keyframes in a HEIF container.
pub fn first_frame(path: &Path, max_dim: Option<u32>) -> Option<FirstFrame> {
    init_ffmpeg().ok()?;
    let mut input = ffmpeg::format::input(path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder()
        .video()
        .ok()?;

    let mut frame = ffmpeg::frame::Video::empty();
    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        if decoder.receive_frame(&mut frame).is_err() {
            continue;
        }

        let (width, height) = (decoder.width(), decoder.height());
        let scale = match max_dim {
            Some(dim) => (dim as f32 / width.max(height) as f32).min(1.0),
            None => 1.0,
        };
        let out_w = ((width as f32 * scale) as u32).max(1);
        let out_h = ((height as f32 * scale) as u32).max(1);

        let mut scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            out_w,
            out_h,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .ok()?;
        let mut rgba_frame = ffmpeg::frame::Video::empty();
        scaler.run(&frame, &mut rgba_frame).ok()?;

        let stride = rgba_frame.stride(0);
        let row_bytes = out_w as usize * 4;
        let data = rgba_frame.data(0);
        let mut pixels = Vec::with_capacity(row_bytes * out_h as usize);
        for row in 0..out_h as usize {
            let offset = row * stride;
            pixels.extend_from_slice(&data[offset..offset + row_bytes]);
        }

        return Some(FirstFrame {
            width: out_w,
            height: out_h,
            native_size: (width, height),
            pixels,
        });
    }
    None
}

/// Decode the first frame, scaled to fit `max_dim`, for the filmstrip.
/// Blocking, run on a worker.
pub fn first_frame_thumb(path: &Path, max_dim: u32) -> Option<crate::media::ThumbData> {
    let frame = first_frame(path, Some(max_dim))?;
    Some(crate::media::ThumbData {
        width: frame.width,
        height: frame.height,
        pixels: frame.pixels,
        original_size: frame.native_size,
    })
}

/// Deletes an extracted temp file once the last session using it drops.
/// Shared by `Arc` across seek/loop respawns of the same video.
pub struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path })
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        // Try inline first since the retry thread dies with the process on
        // exit, so this is the only attempt guaranteed to run.
        if std::fs::remove_file(&self.path).is_ok() {
            return;
        }
        // Decoder threads may still hold the file open for a moment on
        // Windows, so retry off-thread. Anything that survives (crash,
        // hard kill) is swept by `clean_extraction_dir` at next startup.
        let path = self.path.clone();
        std::thread::spawn(move || {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(300));
                if std::fs::remove_file(&path).is_ok() {
                    return;
                }
            }
        });
    }
}

/// Where archive video entries are extracted for playback.
pub fn extraction_dir() -> PathBuf {
    std::env::temp_dir().join("scryglass-video")
}

/// Remove orphaned extractions from crashed or killed sessions.
/// Files still in use are locked and survive the sweep. Blocking,
/// run on a worker at startup.
pub fn clean_extraction_dir() {
    let Ok(entries) = std::fs::read_dir(extraction_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Audio output sample rate. The resampler converts everything to this.
const AUDIO_RATE: u32 = 48000;

/// A UI-poll gap this long (e.g. a window move/resize loop) counts as a
/// stall, so playback freezes instead of drifting.
const STALL: Duration = Duration::from_millis(200);

/// An active playback session. Dropping it stops the decode threads.
pub struct VideoSession {
    frames: mpsc::Receiver<VideoFrame>,
    audio: Option<mpsc::Sender<AudioCmd>>,
    audio_clock_us: Arc<AtomicU64>,
    has_audio: Arc<AtomicBool>,
    /// Set once the first frame shows, releasing the held audio so video
    /// and audio start together.
    video_ready: Arc<AtomicBool>,
    duration_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Seek offset this session started from.
    base: Duration,
    /// Wall-clock fallback for silent files: time playing since last resume.
    started: Option<Instant>,
    accumulated: Duration,
    /// Shared monotonic origin for the UI-liveness timestamp.
    clock_origin: Instant,
    /// Last poll() time as millis since `clock_origin`; the audio watchdog
    /// reads it to spot a suspended UI thread.
    ui_tick_ms: Arc<AtomicU64>,
    /// Last poll() instant, to discount a stall from the silent wall clock.
    last_poll: Option<Instant>,
    /// Whether any frame has been shown. The clock stays at zero until
    /// then, so the slider doesn't creep ahead during decoder warmup and
    /// snap back when the audio clock takes over.
    first_frame_shown: bool,
    /// One decoded frame waiting for its presentation time.
    pending: Option<VideoFrame>,
    pub playing: bool,
    pub looping: bool,
    pub volume: f32,
    pub muted: bool,
    /// Whether hardware decode was requested when this session opened.
    hardware: bool,
    pub path: PathBuf,
    /// Keeps an extracted archive entry's temp file alive across
    /// seek/loop respawns, deleted when the last holder drops.
    pub temp: Option<std::sync::Arc<TempFileGuard>>,
}

impl VideoSession {
    /// Start playback of `path` at `start`, spawning the decode threads.
    pub fn open(
        path: PathBuf,
        start: Duration,
        volume: f32,
        muted: bool,
        looping: bool,
        hardware: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let duration_us = Arc::new(AtomicU64::new(0));
        let video_done = Arc::new(AtomicBool::new(false));
        let audio_clock_us = Arc::new(AtomicU64::new(0));
        let has_audio = Arc::new(AtomicBool::new(false));
        let video_ready = Arc::new(AtomicBool::new(false));
        // Shared with the audio watchdog so it can spot a suspended UI
        // thread and freeze playback instead of letting it drift.
        let clock_origin = Instant::now();
        let ui_tick_ms = Arc::new(AtomicU64::new(0));

        // Audio PCM channel: about half a second of stereo float samples.
        // The decoder blocks when it runs ahead, rodio's thread drains it.
        let (pcm_tx, pcm_rx) = mpsc::sync_channel::<f32>((AUDIO_RATE as usize / 2) * 2);

        let frames = spawn_decode_thread(
            path.clone(),
            start,
            stop.clone(),
            duration_us.clone(),
            video_done.clone(),
            pcm_tx,
            hardware,
        );
        let audio = spawn_audio_output(
            pcm_rx,
            stop.clone(),
            audio_clock_us.clone(),
            has_audio.clone(),
            video_ready.clone(),
            ui_tick_ms.clone(),
            clock_origin,
            if muted { 0.0 } else { volume },
        );

        Self {
            frames,
            audio,
            audio_clock_us,
            has_audio,
            video_ready,
            duration_us,
            video_done,
            stop,
            base: start,
            started: None,
            accumulated: Duration::ZERO,
            clock_origin,
            ui_tick_ms,
            last_poll: None,
            first_frame_shown: false,
            pending: None,
            playing: true,
            looping,
            volume,
            muted,
            hardware,
            path,
            temp: None,
        }
    }

    /// A fresh session on the same file at `start`. Used for seeks and
    /// looping. Carries the temp-file guard so extracted archive entries
    /// survive the respawn, and the known duration so the seek slider
    /// never collapses while the new demuxer spins up.
    pub fn reopen_at(&self, start: Duration) -> Self {
        let mut session = Self::open(
            self.path.clone(),
            start,
            self.volume,
            self.muted,
            self.looping,
            self.hardware,
        );
        session
            .duration_us
            .store(self.duration_us.load(Ordering::Relaxed), Ordering::Relaxed);
        session.temp = self.temp.clone();
        // A seek from a paused video stays paused, showing the new frame.
        if !self.playing {
            session.pause();
        }
        session
    }

    /// Playback clock relative to the session start.
    fn clock(&self) -> Duration {
        if self.has_audio.load(Ordering::Relaxed) {
            Duration::from_micros(self.audio_clock_us.load(Ordering::Relaxed))
        } else if !self.first_frame_shown {
            // Decoder warmup: hold at the start instead of free-running.
            Duration::ZERO
        } else {
            self.accumulated + self.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO)
        }
    }

    /// Absolute playback position in the file.
    pub fn position(&self) -> Duration {
        let position = self.base + self.clock();
        self.duration()
            .map(|duration| position.min(duration))
            .unwrap_or(position)
    }

    /// Total duration, once the container has been opened.
    pub fn duration(&self) -> Option<Duration> {
        let us = self.duration_us.load(Ordering::Relaxed);
        (us > 0).then(|| Duration::from_micros(us))
    }

    /// The newest frame due for display, if any.
    pub fn poll(&mut self) -> Option<VideoFrame> {
        // A paused session still delivers its very first frame, so a seek
        // while paused shows the new position. After that it holds.
        if !self.playing && self.first_frame_shown {
            return None;
        }
        // Mark the UI as alive for the audio watchdog. For silent files
        // (which pace off the wall clock) also discount a long gap, so a
        // suspended UI thread doesn't make the video race on resume.
        let now = Instant::now();
        self.ui_tick_ms.store(
            now.duration_since(self.clock_origin).as_millis() as u64,
            Ordering::Relaxed,
        );
        if !self.has_audio.load(Ordering::Relaxed)
            && let Some(last) = self.last_poll
        {
            let gap = now.duration_since(last);
            if gap > STALL
                && let Some(started) = self.started
            {
                self.started = Some(started + gap);
            }
        }
        self.last_poll = Some(now);

        let clock = self.clock();
        let mut due = None;

        if let Some(pending) = &self.pending {
            if pending.timestamp > clock {
                return None;
            }
            due = self.pending.take();
        }
        loop {
            match self.frames.try_recv() {
                Ok(frame) if frame.timestamp <= clock => due = Some(frame),
                Ok(frame) => {
                    self.pending = Some(frame);
                    break;
                }
                Err(_) => break,
            }
        }
        // The wall-clock fallback starts with the first visible frame, but
        // only while playing. A paused seek shows a frame without letting
        // the clock advance.
        if due.is_some() && !self.first_frame_shown {
            self.first_frame_shown = true;
            // Release the held audio now that the picture is up.
            self.video_ready.store(true, Ordering::Relaxed);
            if self.playing {
                self.started = Some(Instant::now());
            }
        }
        due
    }

    /// Whether decoding finished and every frame has been shown.
    pub fn finished(&self) -> bool {
        self.video_done.load(Ordering::Relaxed)
            && self.pending.is_none()
            && self
                .duration()
                .is_none_or(|duration| self.base + self.clock() >= duration)
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        self.playing = false;
        if let Some(started) = self.started.take() {
            self.accumulated += started.elapsed();
        }
        if let Some(audio) = &self.audio {
            let _ = audio.send(AudioCmd::Pause);
        }
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        self.playing = true;
        self.started = Some(Instant::now());
        // Don't let the silent-clock watchdog read the paused gap as a stall.
        self.last_poll = None;
        if let Some(audio) = &self.audio {
            let _ = audio.send(AudioCmd::Play);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.muted = false;
        self.push_volume();
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.push_volume();
    }

    fn push_volume(&self) {
        if let Some(audio) = &self.audio {
            let effective = if self.muted { 0.0 } else { self.volume };
            let _ = audio.send(AudioCmd::Volume(effective));
        }
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Spawn the decode pipeline: a demux thread routes compressed packets
/// into per-stream queues consumed by independent video and audio decode
/// threads. The pipelines must not share backpressure. A full video
/// frame queue blocking audio decode would starve the sink, freeze the
/// audio clock, and deadlock the whole player.
fn spawn_decode_thread(
    path: PathBuf,
    start: Duration,
    stop: Arc<AtomicBool>,
    duration_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
    pcm_tx: mpsc::SyncSender<f32>,
    hardware: bool,
) -> mpsc::Receiver<VideoFrame> {
    let (tx, rx) = mpsc::sync_channel::<VideoFrame>(4);

    std::thread::spawn(move || {
        // The video decode thread marks `video_done` after its flush. A
        // setup error here must mark it too or the UI waits forever.
        if run_pipeline(
            &path,
            start,
            &stop,
            &duration_us,
            &video_done,
            tx,
            pcm_tx,
            hardware,
        )
        .is_err()
        {
            video_done.store(true, Ordering::Relaxed);
        }
    });

    rx
}

/// Compressed packets are small, so generous queues let the demuxer run
/// ahead across normal A/V interleaving without coupling the streams.
const PACKET_QUEUE: usize = 512;

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    path: &Path,
    start: Duration,
    stop: &Arc<AtomicBool>,
    duration_us: &AtomicU64,
    video_done: &Arc<AtomicBool>,
    tx: mpsc::SyncSender<VideoFrame>,
    pcm_tx: mpsc::SyncSender<f32>,
    hardware: bool,
) -> Result<(), ffmpeg::Error> {
    init_ffmpeg()?;
    let mut input = ffmpeg::format::input(path)?;

    if input.duration() > 0 {
        // `duration()` is in AV_TIME_BASE units (microseconds).
        duration_us.store(input.duration() as u64, Ordering::Relaxed);
    }
    if !start.is_zero() {
        let ts = start.as_micros() as i64;
        input.seek(ts, ..ts)?;
    }
    let base = start.as_secs_f64();

    // --- Video decode thread ---
    let video_stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(ffmpeg::Error::StreamNotFound)?;
    let video_index = video_stream.index();
    let video_tb = f64::from(video_stream.time_base());
    let mut video_context =
        ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?;
    // Decode on the GPU when possible; the decoder falls back to software
    // inside `get_hw_format` for unsupported codecs. Frame threading only
    // helps the software path, so set it when hardware isn't attached. A
    // single thread can't keep up at 4K (count 0 means one per logical CPU).
    let hw_active = hardware && try_init_hw(&mut video_context);
    if !hw_active {
        video_context.set_threading(ffmpeg::codec::threading::Config::kind(
            ffmpeg::codec::threading::Type::Frame,
        ));
    }
    let mut video_decoder = video_context.decoder().video()?;

    let (video_pkt_tx, video_pkt_rx) =
        mpsc::sync_channel::<ffmpeg::codec::packet::Packet>(PACKET_QUEUE);
    let video_stop = stop.clone();
    let video_finished = video_done.clone();
    std::thread::spawn(move || {
        let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
        let mut frame = ffmpeg::frame::Video::empty();
        let mut sw_frame = ffmpeg::frame::Video::empty();
        // The first kept frame is time zero: the seek target, not the
        // keyframe before it.
        let mut origin: Option<f64> = None;
        let mut drain = |decoder: &mut ffmpeg::decoder::Video,
                         scaler: &mut Option<ffmpeg::software::scaling::Context>|
         -> Result<(), ()> {
            while decoder.receive_frame(&mut frame).is_ok() {
                if video_stop.load(Ordering::Relaxed) {
                    return Err(());
                }
                let pts = frame.pts().unwrap_or(0) as f64 * video_tb;
                // Drop frames before the seek target; they exist only for
                // the reference chain.
                if pts < base {
                    continue;
                }
                // Rebase the first kept frame to zero so it shows at once.
                let relative = pts - *origin.get_or_insert(pts);
                // Hardware frames live in GPU memory: copy them down to
                // system memory (NV12) before the planes can be read. A
                // failed copy drops just that frame.
                let source = if is_hw_frame(&frame) {
                    match download_frame(&frame) {
                        Some(downloaded) => {
                            sw_frame = downloaded;
                            &sw_frame
                        }
                        None => continue,
                    }
                } else {
                    &frame
                };
                send_video_frame(scaler, source, relative, &tx)?;
            }
            Ok(())
        };

        while let Ok(packet) = video_pkt_rx.recv() {
            if video_stop.load(Ordering::Relaxed) {
                return;
            }
            if video_decoder.send_packet(&packet).is_err() {
                continue;
            }
            if drain(&mut video_decoder, &mut scaler).is_err() {
                return;
            }
        }
        // Demuxer hung up: flush the decoder.
        let _ = video_decoder.send_eof();
        let _ = drain(&mut video_decoder, &mut scaler);
        video_finished.store(true, Ordering::Relaxed);
    });

    // --- Audio decode thread (optional) ---
    let audio_stream = input.streams().best(ffmpeg::media::Type::Audio);
    let audio_index = audio_stream.as_ref().map(|s| s.index());
    let audio_pkt_tx = match &audio_stream {
        Some(stream) => {
            let mut audio_decoder =
                ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
                    .decoder()
                    .audio()?;
            let (pkt_tx, pkt_rx) =
                mpsc::sync_channel::<ffmpeg::codec::packet::Packet>(PACKET_QUEUE);
            let audio_stop = stop.clone();
            std::thread::spawn(move || {
                let mut resampler: Option<ffmpeg::software::resampling::Context> = None;
                let mut frame = ffmpeg::frame::Audio::empty();
                while let Ok(packet) = pkt_rx.recv() {
                    if audio_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if audio_decoder.send_packet(&packet).is_err() {
                        continue;
                    }
                    while audio_decoder.receive_frame(&mut frame).is_ok() {
                        if audio_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if send_audio_frame(&mut resampler, &audio_decoder, &frame, &pcm_tx)
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                // pcm_tx drops here, ending the rodio source.
            });
            Some(pkt_tx)
        }
        None => None,
    };

    // --- Demux loop: route packets, never decode ---
    for (stream, packet) in input.packets() {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if stream.index() == video_index {
            if video_pkt_tx.send(packet).is_err() {
                return Ok(());
            }
        } else if Some(stream.index()) == audio_index
            && let Some(tx) = &audio_pkt_tx
            && tx.send(packet).is_err()
        {
            return Ok(());
        }
    }
    // Senders drop here and the decode threads flush and finish.
    Ok(())
}

/// Convert one decoded frame to planar I420 if needed and push it
/// (blocking on backpressure). The GPU does the color conversion, so the
/// CPU only moves luma and chroma planes (1.5 bytes per pixel).
fn send_video_frame(
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    frame: &ffmpeg::frame::Video,
    relative_secs: f64,
    tx: &mpsc::SyncSender<VideoFrame>,
) -> Result<(), ()> {
    use ffmpeg::format::Pixel;

    // I420 (software) and NV12 (hardware download) upload straight to the
    // GPU. Anything else (10-bit, 4:2:2, 4:4:4) converts once into I420,
    // far cheaper than the old full RGBA conversion and rare in practice.
    let mut converted = ffmpeg::frame::Video::empty();
    let (src, format) = match frame.format() {
        Pixel::YUV420P => (frame, YuvFormat::I420),
        Pixel::NV12 => (frame, YuvFormat::Nv12),
        _ => {
            if scaler.is_none() {
                *scaler = ffmpeg::software::scaling::Context::get(
                    frame.format(),
                    frame.width(),
                    frame.height(),
                    Pixel::YUV420P,
                    frame.width(),
                    frame.height(),
                    ffmpeg::software::scaling::Flags::BILINEAR,
                )
                .ok();
            }
            let Some(scaler) = scaler.as_mut() else {
                return Ok(());
            };
            if scaler.run(frame, &mut converted).is_err() {
                return Ok(());
            }
            (&converted, YuvFormat::I420)
        }
    };

    let width = src.width();
    let height = src.height();
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);

    let y = copy_plane(src.data(0), src.stride(0), width as usize, height as usize);
    let (u, v) = match format {
        YuvFormat::I420 => (
            copy_plane(
                src.data(1),
                src.stride(1),
                chroma_width as usize,
                chroma_height as usize,
            ),
            copy_plane(
                src.data(2),
                src.stride(2),
                chroma_width as usize,
                chroma_height as usize,
            ),
        ),
        // NV12 keeps one interleaved UV plane: two bytes per chroma sample.
        YuvFormat::Nv12 => (
            copy_plane(
                src.data(1),
                src.stride(1),
                chroma_width as usize * 2,
                chroma_height as usize,
            ),
            Vec::new(),
        ),
    };

    let matrix = match src.color_space() {
        ffmpeg::util::color::Space::BT709 => YuvMatrix::Bt709,
        ffmpeg::util::color::Space::BT470BG | ffmpeg::util::color::Space::SMPTE170M => {
            YuvMatrix::Bt601
        }
        // Unspecified: HD and up is almost always BT.709, SD is BT.601.
        _ => {
            if height >= 720 {
                YuvMatrix::Bt709
            } else {
                YuvMatrix::Bt601
            }
        }
    };
    let range = match src.color_range() {
        ffmpeg::util::color::Range::JPEG => YuvRange::Full,
        _ => YuvRange::Limited,
    };

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    tx.send(VideoFrame {
        id,
        width,
        height,
        chroma_width,
        chroma_height,
        format,
        y,
        u,
        v,
        matrix,
        range,
        timestamp: Duration::from_secs_f64(relative_secs),
    })
    .map_err(|_| ())
}
