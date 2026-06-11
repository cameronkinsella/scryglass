//! Video playback via a spawned ffmpeg binary (cargo feature `video`).
//!
//! Two ffmpeg processes per session: one decodes video to raw RGB frames
//! over a pipe (with backpressure through a bounded channel), the other
//! decodes audio to PCM consumed by a rodio sink on a dedicated thread.
//! Audio is the master clock. Files without audio fall back to a
//! pause-aware wall clock. Seeking respawns the processes at the target
//! position (`-ss` before `-i` is a fast keyframe seek).

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;

/// Video container extensions offered in the file list.
pub const EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

/// Whether this path looks like a video file.
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
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

/// An active playback session. Dropping it stops both ffmpeg processes.
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
    /// One decoded frame waiting for its presentation time.
    pending: Option<VideoFrame>,
    pub playing: bool,
    pub looping: bool,
    pub volume: f32,
    pub muted: bool,
    pub path: PathBuf,
}

impl VideoSession {
    /// Start playback of `path` at `start`, spawning the decode threads.
    pub fn open(path: PathBuf, start: Duration, volume: f32, muted: bool, looping: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let duration_us = Arc::new(AtomicU64::new(0));
        let video_done = Arc::new(AtomicBool::new(false));
        let audio_clock_us = Arc::new(AtomicU64::new(0));
        let has_audio = Arc::new(AtomicBool::new(false));

        let frames = spawn_video_thread(
            path.clone(),
            start,
            stop.clone(),
            duration_us.clone(),
            video_done.clone(),
        );
        let audio = spawn_audio_thread(
            path.clone(),
            start,
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
            started: Some(Instant::now()),
            accumulated: Duration::ZERO,
            pending: None,
            playing: true,
            looping,
            volume,
            muted,
            path,
        }
    }

    /// Playback clock relative to the session start.
    fn clock(&self) -> Duration {
        if self.has_audio.load(Ordering::Relaxed) {
            Duration::from_micros(self.audio_clock_us.load(Ordering::Relaxed))
        } else {
            self.accumulated + self.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO)
        }
    }

    /// Absolute playback position in the file.
    pub fn position(&self) -> Duration {
        self.base + self.clock()
    }

    /// Total duration, once ffmpeg has reported it.
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
        due
    }

    /// Whether decoding finished and every frame has been shown.
    pub fn finished(&self) -> bool {
        self.video_done.load(Ordering::Relaxed) && self.pending.is_none()
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

/// Decode video frames into a bounded channel (backpressure keeps memory
/// flat while paused since the producer blocks until frames are consumed).
fn spawn_video_thread(
    path: PathBuf,
    start: Duration,
    stop: Arc<AtomicBool>,
    duration_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
) -> mpsc::Receiver<VideoFrame> {
    let (tx, rx) = mpsc::sync_channel::<VideoFrame>(4);

    std::thread::spawn(move || {
        let mut command = FfmpegCommand::new();
        command.hide_banner();
        if !start.is_zero() {
            command.seek(format!("{:.3}", start.as_secs_f64()));
        }
        command.input(path.to_string_lossy()).no_audio().rawvideo();

        let Ok(mut child) = command.spawn() else {
            video_done.store(true, Ordering::Relaxed);
            return;
        };
        let Ok(events) = child.iter() else {
            let _ = child.kill();
            video_done.store(true, Ordering::Relaxed);
            return;
        };

        for event in events {
            if stop.load(Ordering::Relaxed) {
                let _ = child.kill();
                return;
            }
            match event {
                FfmpegEvent::ParsedDuration(d) => {
                    duration_us.store((d.duration * 1e6) as u64, Ordering::Relaxed);
                }
                FfmpegEvent::OutputFrame(frame) => {
                    // rgb24 → RGBA8.
                    let mut rgba = Vec::with_capacity(frame.data.len() / 3 * 4);
                    for px in frame.data.chunks_exact(3) {
                        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                    }
                    let sent = tx.send(VideoFrame {
                        width: frame.width,
                        height: frame.height,
                        rgba,
                        timestamp: Duration::from_secs_f32(frame.timestamp.max(0.0)),
                    });
                    if sent.is_err() {
                        // Session dropped.
                        let _ = child.kill();
                        return;
                    }
                }
                _ => {}
            }
        }
        video_done.store(true, Ordering::Relaxed);
    });

    rx
}

/// PCM samples streamed straight from the ffmpeg pipe into rodio.
struct PcmPipe {
    reader: BufReader<std::process::ChildStdout>,
}

impl Iterator for PcmPipe {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let mut bytes = [0u8; 4];
        self.reader.read_exact(&mut bytes).ok()?;
        Some(f32::from_le_bytes(bytes))
    }
}

impl rodio::Source for PcmPipe {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Play the file's audio through rodio, publishing the sink position as
/// the master clock. The `OutputStream` must live on its own thread (it
/// is not `Send`), so commands arrive over a channel.
fn spawn_audio_thread(
    path: PathBuf,
    start: Duration,
    stop: Arc<AtomicBool>,
    clock_us: Arc<AtomicU64>,
    has_audio: Arc<AtomicBool>,
    volume: f32,
) -> Option<mpsc::Sender<AudioCmd>> {
    let (tx, rx) = mpsc::channel::<AudioCmd>();

    std::thread::spawn(move || {
        let mut child: Child = match Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(["-ss", &format!("{:.3}", start.as_secs_f64())])
            .arg("-i")
            .arg(&path)
            .args(["-vn", "-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return,
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return;
        };

        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            let _ = child.kill();
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            let _ = child.kill();
            return;
        };
        sink.set_volume(volume);
        sink.append(PcmPipe {
            reader: BufReader::new(stdout),
        });

        loop {
            if stop.load(Ordering::Relaxed) {
                sink.stop();
                let _ = child.kill();
                return;
            }
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    AudioCmd::Volume(v) => sink.set_volume(v),
                    AudioCmd::Pause => sink.pause(),
                    AudioCmd::Play => sink.play(),
                }
            }
            // Silent files produce no samples: the sink stays empty and
            // the session falls back to its wall clock.
            if !sink.empty() {
                has_audio.store(true, Ordering::Relaxed);
                clock_us.store(sink.get_pos().as_micros() as u64, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    Some(tx)
}
