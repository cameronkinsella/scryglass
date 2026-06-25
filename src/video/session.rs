//! The playback session. It owns the decode threads, the master clock, and
//! the temp-file lifecycle for extracted archive entries. Polling hands the
//! UI the newest frame due. Dropping the session stops the threads.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::audio::{AudioCmd, spawn_audio_output};
use super::decode::spawn_decode_thread;
use super::{AUDIO_RATE, STALL, VideoFrame};

/// How long the decoder may run dry before the keep-alive engages.
const DECODE_BEHIND_GAP: Duration = Duration::from_millis(200);

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
    /// One frame's duration in microseconds, for frame stepping. 0 until the
    /// stream is opened.
    frame_us: Arc<AtomicU64>,
    video_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Seek offset this session started from.
    base: Duration,
    /// Wall-clock fallback for silent files: time playing since last resume.
    started: Option<Instant>,
    accumulated: Duration,
    /// Shared monotonic origin for the UI-liveness timestamp.
    clock_origin: Instant,
    /// Last poll() time as millis since `clock_origin`. The audio watchdog
    /// reads it to spot a suspended UI thread.
    ui_tick_ms: Arc<AtomicU64>,
    /// Last poll() instant, to discount a stall from the silent wall clock.
    last_poll: Option<Instant>,
    /// Whether any frame has been shown. The clock holds at zero until then,
    /// so the slider doesn't creep ahead and snap back during warmup.
    first_frame_shown: bool,
    /// One decoded frame waiting for its presentation time.
    pending: Option<VideoFrame>,
    /// When the decoder first ran dry, for keep-alive stutter detection.
    starved_since: Option<Instant>,
    pub playing: bool,
    looping: Arc<AtomicBool>,
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
        let frame_us = Arc::new(AtomicU64::new(0));
        let video_done = Arc::new(AtomicBool::new(false));
        let audio_clock_us = Arc::new(AtomicU64::new(0));
        let has_audio = Arc::new(AtomicBool::new(false));
        let video_ready = Arc::new(AtomicBool::new(false));
        // Shared with the audio watchdog so it can spot a suspended UI
        // thread and freeze playback instead of letting it drift.
        let clock_origin = Instant::now();
        let ui_tick_ms = Arc::new(AtomicU64::new(0));
        let looping = Arc::new(AtomicBool::new(looping));

        // Audio PCM channel: about half a second of stereo float samples.
        // The decoder blocks when it runs ahead, rodio's thread drains it.
        let (pcm_tx, pcm_rx) = mpsc::sync_channel::<f32>((AUDIO_RATE as usize / 2) * 2);

        let frames = spawn_decode_thread(
            path.clone(),
            start,
            stop.clone(),
            duration_us.clone(),
            frame_us.clone(),
            video_done.clone(),
            pcm_tx,
            hardware,
            looping.clone(),
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
            frame_us,
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
            starved_since: None,
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
            self.looping(),
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
        compute_clock(
            self.has_audio.load(Ordering::Relaxed),
            self.first_frame_shown,
            self.audio_clock_us.load(Ordering::Relaxed),
            self.accumulated,
            self.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO),
        )
    }

    /// Absolute playback position in the file.
    pub fn position(&self) -> Duration {
        loop_position(self.base, self.clock(), self.duration(), self.looping())
    }

    /// Total duration, once the container has been opened.
    pub fn duration(&self) -> Option<Duration> {
        let us = self.duration_us.load(Ordering::Relaxed);
        (us > 0).then(|| Duration::from_micros(us))
    }

    /// One frame's duration, once the video stream has been opened.
    pub fn frame_duration(&self) -> Option<Duration> {
        let us = self.frame_us.load(Ordering::Relaxed);
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
        // Healthy playback keeps a frame queued ahead; a sustained empty
        // queue under hardware decode is the idle-GPU stutter. See
        // `gpu_keepalive`.
        if self.pending.is_some() {
            self.starved_since = None;
        } else if self.playing
            && self.first_frame_shown
            && self.hardware
            && !self.video_done.load(Ordering::Relaxed)
        {
            let since = *self.starved_since.get_or_insert(now);
            if now.duration_since(since) > DECODE_BEHIND_GAP {
                crate::gpu_keepalive::flag_decode_behind();
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

    pub fn looping(&self) -> bool {
        self.looping.load(Ordering::Relaxed)
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping.store(looping, Ordering::Relaxed);
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

/// The in-file position to show. While looping, the clock keeps climbing past
/// the end, so wrap it back into `[0, duration)` for a slider that sweeps once
/// per loop. Otherwise clamp to the end (an unknown duration stays unclamped).
fn loop_position(
    base: Duration,
    clock: Duration,
    duration: Option<Duration>,
    looping: bool,
) -> Duration {
    let position = base + clock;
    let Some(duration) = duration else {
        return position;
    };
    if looping && !duration.is_zero() {
        Duration::from_nanos((position.as_nanos() % duration.as_nanos()) as u64)
    } else {
        position.min(duration)
    }
}

/// The playback clock from its inputs: the audio sink position once audio
/// is flowing, zero during decoder warmup before the first frame, otherwise
/// the accumulated wall-clock time plus the current run.
fn compute_clock(
    has_audio: bool,
    first_frame_shown: bool,
    audio_us: u64,
    accumulated: Duration,
    playing_elapsed: Duration,
) -> Duration {
    if has_audio {
        Duration::from_micros(audio_us)
    } else if !first_frame_shown {
        Duration::ZERO
    } else {
        accumulated + playing_elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_position_is_the_clock_when_audio_flows() {
        let clock = compute_clock(
            true,
            true,
            1_500_000,
            Duration::from_secs(99),
            Duration::from_secs(99),
        );
        assert_eq!(clock, Duration::from_micros(1_500_000));
    }

    #[test]
    fn clock_holds_at_zero_during_warmup() {
        let clock = compute_clock(
            false,
            false,
            0,
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert_eq!(clock, Duration::ZERO);
    }

    #[test]
    fn silent_files_run_off_the_wall_clock() {
        let clock = compute_clock(
            false,
            true,
            0,
            Duration::from_secs(2),
            Duration::from_millis(500),
        );
        assert_eq!(clock, Duration::from_millis(2500));
    }

    #[test]
    fn position_clamps_to_the_end_when_not_looping() {
        let p = loop_position(
            Duration::ZERO,
            Duration::from_secs(9),
            Some(Duration::from_secs(5)),
            false,
        );
        assert_eq!(p, Duration::from_secs(5));
    }

    #[test]
    fn a_looping_position_within_the_first_pass_is_unwrapped() {
        let p = loop_position(
            Duration::ZERO,
            Duration::from_secs(3),
            Some(Duration::from_secs(5)),
            true,
        );
        assert_eq!(p, Duration::from_secs(3));
    }

    #[test]
    fn a_looping_position_wraps_past_the_end() {
        // 2.5 loops of a 5s clip reads as 2.5s into the current pass.
        let p = loop_position(
            Duration::ZERO,
            Duration::from_millis(12_500),
            Some(Duration::from_secs(5)),
            true,
        );
        assert_eq!(p, Duration::from_millis(2_500));
    }

    #[test]
    fn position_is_unclamped_without_a_known_duration() {
        let p = loop_position(Duration::ZERO, Duration::from_secs(9), None, true);
        assert_eq!(p, Duration::from_secs(9));
    }
}
