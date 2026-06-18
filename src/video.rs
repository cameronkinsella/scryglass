//! Video playback via FFmpeg's libraries (cargo feature `video`).
//!
//! One session thread demuxes the file in-process: video packets decode
//! and rescale to RGBA (frames flow through a bounded channel for
//! backpressure), audio packets decode and resample to f32 PCM consumed
//! by a rodio sink on its own thread. The sink's position is the master
//! clock. Files without audio fall back to a pause-aware wall clock.
//! Seeking opens a fresh session at the target (`avformat` seek before
//! decode begins). No external processes are involved anywhere.

use std::num::{NonZeroU16, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;

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

/// A decoded frame ready for GPU upload.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Presentation time relative to the session start (the seek point).
    pub timestamp: Duration,
}

enum AudioCmd {
    Volume(f32),
    Pause,
    Play,
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

/// An active playback session. Dropping it stops the decode threads.
pub struct VideoSession {
    frames: mpsc::Receiver<VideoFrame>,
    audio: Option<mpsc::Sender<AudioCmd>>,
    audio_clock_us: Arc<AtomicU64>,
    has_audio: Arc<AtomicBool>,
    duration_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Seek offset this session started from.
    base: Duration,
    /// Wall-clock fallback for silent files: time playing since last resume.
    started: Option<Instant>,
    accumulated: Duration,
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
    pub path: PathBuf,
    /// Keeps an extracted archive entry's temp file alive across
    /// seek/loop respawns, deleted when the last holder drops.
    pub temp: Option<std::sync::Arc<TempFileGuard>>,
}

impl VideoSession {
    /// Start playback of `path` at `start`, spawning the decode threads.
    pub fn open(path: PathBuf, start: Duration, volume: f32, muted: bool, looping: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let duration_us = Arc::new(AtomicU64::new(0));
        let video_done = Arc::new(AtomicBool::new(false));
        let audio_clock_us = Arc::new(AtomicU64::new(0));
        let has_audio = Arc::new(AtomicBool::new(false));

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
        );
        let audio = spawn_audio_output(
            pcm_rx,
            stop.clone(),
            audio_clock_us.clone(),
            has_audio.clone(),
            if muted { 0.0 } else { volume },
        );

        Self {
            frames,
            audio,
            audio_clock_us,
            has_audio,
            duration_us,
            video_done,
            stop,
            base: start,
            started: None,
            accumulated: Duration::ZERO,
            first_frame_shown: false,
            pending: None,
            playing: true,
            looping,
            volume,
            muted,
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
        );
        session
            .duration_us
            .store(self.duration_us.load(Ordering::Relaxed), Ordering::Relaxed);
        session.temp = self.temp.clone();
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
        if !self.playing {
            return None;
        }
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
        // The wall-clock fallback starts with the first visible frame.
        if due.is_some() && !self.first_frame_shown {
            self.first_frame_shown = true;
            self.started = Some(Instant::now());
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
) -> mpsc::Receiver<VideoFrame> {
    let (tx, rx) = mpsc::sync_channel::<VideoFrame>(4);

    std::thread::spawn(move || {
        // The video decode thread marks `video_done` after its flush. A
        // setup error here must mark it too or the UI waits forever.
        if run_pipeline(&path, start, &stop, &duration_us, &video_done, tx, pcm_tx).is_err() {
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
    let mut video_decoder =
        ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?
            .decoder()
            .video()?;

    let (video_pkt_tx, video_pkt_rx) =
        mpsc::sync_channel::<ffmpeg::codec::packet::Packet>(PACKET_QUEUE);
    let video_stop = stop.clone();
    let video_finished = video_done.clone();
    std::thread::spawn(move || {
        let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
        let mut frame = ffmpeg::frame::Video::empty();
        let mut drain = |decoder: &mut ffmpeg::decoder::Video,
                         scaler: &mut Option<ffmpeg::software::scaling::Context>|
         -> Result<(), ()> {
            while decoder.receive_frame(&mut frame).is_ok() {
                if video_stop.load(Ordering::Relaxed) {
                    return Err(());
                }
                let pts = frame.pts().unwrap_or(0) as f64 * video_tb;
                let relative = (pts - base).max(0.0);
                send_video_frame(scaler, decoder, &frame, relative, &tx)?;
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

/// Rescale one decoded frame to RGBA and push it (blocking on backpressure).
fn send_video_frame(
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    decoder: &ffmpeg::decoder::Video,
    frame: &ffmpeg::frame::Video,
    relative_secs: f64,
    tx: &mpsc::SyncSender<VideoFrame>,
) -> Result<(), ()> {
    // Lazy init: some streams only report a real pixel format with the
    // first frame.
    if scaler.is_none() {
        *scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGBA,
            decoder.width(),
            decoder.height(),
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .ok();
    }
    let Some(scaler) = scaler.as_mut() else {
        return Ok(());
    };

    let mut rgba_frame = ffmpeg::frame::Video::empty();
    if scaler.run(frame, &mut rgba_frame).is_err() {
        return Ok(());
    }

    let (width, height) = (rgba_frame.width(), rgba_frame.height());
    let stride = rgba_frame.stride(0);
    let row_bytes = width as usize * 4;
    let data = rgba_frame.data(0);

    // Rows can carry stride padding, copy row by row.
    let mut rgba = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let offset = row * stride;
        rgba.extend_from_slice(&data[offset..offset + row_bytes]);
    }

    tx.send(VideoFrame {
        width,
        height,
        rgba,
        timestamp: Duration::from_secs_f64(relative_secs),
    })
    .map_err(|_| ())
}

/// Resample one decoded audio frame to interleaved f32 stereo and push
/// each sample (blocking on backpressure).
fn send_audio_frame(
    resampler: &mut Option<ffmpeg::software::resampling::Context>,
    decoder: &ffmpeg::decoder::Audio,
    frame: &ffmpeg::frame::Audio,
    pcm_tx: &mpsc::SyncSender<f32>,
) -> Result<(), ()> {
    if resampler.is_none() {
        *resampler = ffmpeg::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            ffmpeg::ChannelLayout::STEREO,
            AUDIO_RATE,
        )
        .ok();
    }
    let Some(resampler) = resampler.as_mut() else {
        return Ok(());
    };

    let mut resampled = ffmpeg::frame::Audio::empty();
    if resampler.run(frame, &mut resampled).is_err() {
        return Ok(());
    }

    // Packed f32 stereo: plane 0 holds interleaved bytes.
    let sample_bytes = resampled.samples() * 2 * size_of::<f32>();
    let data = &resampled.data(0)[..sample_bytes];
    for chunk in data.chunks_exact(4) {
        let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if pcm_tx.send(sample).is_err() {
            return Err(());
        }
    }
    Ok(())
}

/// PCM samples streamed from the decode thread into rodio. `recv()`
/// blocks rodio's mixer until samples arrive, exactly like a slow file
/// read would. `None` on disconnect ends the source.
struct PcmChannel {
    rx: mpsc::Receiver<f32>,
}

impl Iterator for PcmChannel {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.rx.recv().ok()
    }
}

impl rodio::Source for PcmChannel {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> NonZeroU16 {
        NonZeroU16::new(2).expect("stereo channel count is non-zero")
    }
    fn sample_rate(&self) -> NonZeroU32 {
        NonZeroU32::new(AUDIO_RATE).expect("audio rate is non-zero")
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Play decoded PCM through rodio, publishing the player position as the
/// master clock. The device sink lives on its own thread, so commands
/// arrive over a channel.
fn spawn_audio_output(
    pcm_rx: mpsc::Receiver<f32>,
    stop: Arc<AtomicBool>,
    clock_us: Arc<AtomicU64>,
    has_audio: Arc<AtomicBool>,
    volume: f32,
) -> Option<mpsc::Sender<AudioCmd>> {
    let (tx, rx) = mpsc::channel::<AudioCmd>();

    std::thread::spawn(move || {
        let Ok(mut device_sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
            // No audio device: drain the channel so the decoder never
            // blocks, and let the wall clock pace playback.
            while !stop.load(Ordering::Relaxed) {
                while pcm_rx.try_recv().is_ok() {}
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        };
        device_sink.log_on_drop(false);
        let player = rodio::Player::connect_new(device_sink.mixer());
        player.set_volume(volume);
        player.append(PcmChannel { rx: pcm_rx });
        let mut playing = true;
        let mut ended_clock: Option<(Duration, Option<Instant>)> = None;

        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    AudioCmd::Volume(v) => player.set_volume(v),
                    AudioCmd::Pause => {
                        if let Some((position, Some(started))) = ended_clock {
                            ended_clock = Some((position + started.elapsed(), None));
                        }
                        playing = false;
                        player.pause();
                    }
                    AudioCmd::Play => {
                        if let Some((position, None)) = ended_clock {
                            ended_clock = Some((position, Some(Instant::now())));
                        }
                        playing = true;
                        player.play();
                    }
                }
            }
            // Silent files feed no samples: the player stays at zero and
            // the session falls back to its wall clock.
            let pos = player.get_pos();
            if pos > Duration::ZERO {
                has_audio.store(true, Ordering::Relaxed);
                clock_us.store(pos.as_micros() as u64, Ordering::Relaxed);
            }
            if player.empty() && has_audio.load(Ordering::Relaxed) {
                let (position, started) =
                    ended_clock.get_or_insert_with(|| (pos, playing.then(Instant::now)));
                let position =
                    *position + started.map(|started| started.elapsed()).unwrap_or_default();
                clock_us.store(position.as_micros() as u64, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    Some(tx)
}
