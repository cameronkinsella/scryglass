use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use iced::Task;
use iced::time::Instant;

use crate::app::state::{DisplayedImage, Viewer};
use crate::app::viewer_math::compute_zoom;
use crate::app::{Message, VIDEO_CONTROLS_TIMEOUT, VideoMessage};
use crate::components::toasts::ToastKind;
use crate::config::ZoomMode;
use crate::media::archive::ArchiveIndex;
use crate::media::pipeline::Source;

use super::media_tasks::fire_archive_video_thumb;
use super::push_toast;
use super::settings::save_config;
use crate::app::{Shared, Window};

/// Begin video playback for the current file: open a session directly
/// for filesystem files, or extract the archive entry to a temp file
/// first (FFmpeg needs a real file, the spinner covers the wait).
pub(crate) fn start_video(
    viewer: &mut Viewer,
    current: PathBuf,
    volume: f32,
    muted: bool,
    loop_video: bool,
    hardware: bool,
) -> Task<Message> {
    // Show the controls briefly on open, like most players.
    viewer.video.controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    match &viewer.source {
        Source::Fs => {
            viewer.video.session = Some(crate::video::VideoSession::open(
                current,
                std::time::Duration::ZERO,
                volume,
                muted,
                loop_video,
                hardware,
            ));
            Task::none()
        }
        Source::Archive(index) => {
            if viewer.video.extracting.as_deref() == Some(&*current) {
                return Task::none();
            }
            viewer.video.extracting = Some(current.clone());
            fire_video_extract(index.clone(), current)
        }
    }
}

/// Extract an archive video entry to a uniquely-named temp file,
/// off-thread. The whole entry is written out before playback starts.
pub(crate) fn fire_video_extract(index: Arc<ArchiveIndex>, entry: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let e = entry.clone();
            let result = tokio::task::spawn_blocking(move || {
                static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let bytes = index.read(&e).map_err(|err| err.to_string())?;
                let dir = crate::video::extraction_dir();
                std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
                let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let name = e.file_name().unwrap_or_default().to_string_lossy();
                let file = dir.join(format!("{}-{unique}-{name}", std::process::id()));
                std::fs::write(&file, bytes).map_err(|err| err.to_string())?;
                Ok(file)
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|r| r);
            (entry, result)
        },
        |(entry, result)| Message::VideoControls(VideoMessage::Extracted { entry, result }),
    )
}

pub(crate) fn tick(win: &mut Window, shared: &mut Shared) -> Task<Message> {
    // A volume drag or nudge burst arms a save deadline. Fire it here on the
    // update thread once it settles, so the write captures the live config
    // rather than a stale clone from arm time. A different setting changed
    // during the settle then survives instead of being reverted.
    let save = poll_volume_save(shared);
    Task::batch([save, tick_frame(win, shared)])
}

fn tick_frame(win: &mut Window, shared: &mut Shared) -> Task<Message> {
    let zoom_mode = shared.config.standard.display.zoom_mode;
    let viewport = win.viewport_size;
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };

    // Ease the control bar toward visible/hidden once per tick.
    let playing = viewer.video.session.as_ref().is_some_and(|s| s.playing);
    let controls_alive = viewer
        .video
        .controls_until
        .is_some_and(|until| Instant::now() < until);
    let target = if crate::components::video_controls::controls_visible(
        playing,
        viewer.video.seek_drag.is_some(),
        controls_alive,
    ) {
        1.0
    } else {
        0.0
    };
    viewer.video.controls_opacity = crate::components::video_controls::ease_toward(
        viewer.video.controls_opacity,
        target,
        crate::app::CONTROLS_FADE_STEP,
    );

    let Some(session) = viewer.video.session.as_mut() else {
        return Task::none();
    };

    // A decode setup failure (unopenable file, no video stream, codec init)
    // never delivers a frame. Surface it the way a failed still is surfaced
    // and tear the session down. Leaving it would spin the loading spinner
    // forever, and reopening it under looping would respawn the decode
    // threads and audio sink every tick.
    if let Some(err) = session.failed() {
        let path = viewer.nav.current().to_path_buf();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let message = format!("{name}\n\n{err}");
        viewer.failed_loads.insert(path.clone(), message.clone());
        viewer.displayed = DisplayedImage::Error { message };
        viewer.displayed_path = Some(path);
        viewer.pending_since = None;
        viewer.video.reset();
        return Task::none();
    }

    let Some(frame) = session.poll() else {
        // Only a session with nothing left to show is finished, since
        // queued frames still drain through poll() above.
        if session.finished() {
            // Only a session that showed a frame may reopen. A zero-frame
            // session (a failure the error slot missed) would respawn its
            // decode pipeline every tick.
            if session.looping() && session.showed_frame() {
                viewer.video.session = Some(session.reopen_at(std::time::Duration::ZERO));
            } else if session.playing {
                session.pause();
            }
        }
        return Task::none();
    };

    let path = viewer.nav.current().to_path_buf();
    let (w, h) = (frame.width, frame.height);
    // First visible frame of this video: set the fit zoom like the
    // still-image path does, then hand later frames straight through.
    let first = !matches!(viewer.displayed, DisplayedImage::Video { .. });
    if first && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
        viewer.pan = (0.0, 0.0);
    }
    viewer.video.frame = Some(Arc::new(frame));
    viewer.displayed = DisplayedImage::Video {
        original_size: (w, h),
    };
    viewer.displayed_path = Some(path);
    viewer.pending_since = None;
    Task::none()
}

pub(crate) fn extracted(
    win: &mut Window,
    shared: &mut Shared,
    entry: PathBuf,
    result: Result<PathBuf, String>,
) -> Task<Message> {
    let video_volume = shared.config.standard.video.volume;
    let video_muted = shared.config.standard.video.muted;
    let video_loop = shared.config.standard.video.looping;
    let hardware = shared.config.standard.video.hardware_decode;
    let minimized = win.minimized;
    let focused = win.focused;
    let pause_minimized = shared.config.advanced.resource.minimized.video.pause;
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    if viewer.video.extracting.as_deref() == Some(&*entry) {
        viewer.video.extracting = None;
    }
    // Navigated away while extracting: discard the temp file.
    if viewer.nav.current() != entry {
        if let Ok(temp) = result {
            drop(crate::video::TempFileGuard::new(temp));
        }
        return Task::none();
    }
    match result {
        Err(e) => {
            viewer.pending_since = None;
            push_toast(
                win,
                shared,
                ToastKind::Error,
                format!("Couldn't play video: {e}"),
            )
        }
        Ok(temp) => {
            let guard = crate::video::TempFileGuard::new(temp.clone());
            let mut session = crate::video::VideoSession::open(
                temp.clone(),
                std::time::Duration::ZERO,
                video_volume,
                video_muted,
                video_loop,
                hardware,
            );
            session.temp = Some(guard.clone());
            // Extraction can outlast a minimize. The minimize pause ran while
            // no session existed, so apply it to the fresh session here, the
            // way the `Minimized` handler would have.
            let pause_now = minimized && pause_minimized;
            if pause_now {
                session.pause();
            }
            viewer.video.session = Some(session);
            // The entry has no real path of its own, so grab its thumbnail from
            // the file extracted for playback (none otherwise).
            let thumb = fire_archive_video_thumb(
                &shared.pipeline,
                &shared.thumbs,
                viewer,
                entry,
                temp,
                guard,
            );
            if pause_now {
                win.video_resumes_on_restore = true;
            }
            // A backgrounded window's decay armed while no session existed, so
            // it classified the window as a still and never armed the video
            // stage. Re-arm so the session release schedule governs it.
            if minimized || !focused {
                return Task::batch([thumb, super::decay::restart_decay(win, shared)]);
            }
            thumb
        }
    }
}

pub(crate) fn play_pause(win: &mut Window, _shared: &mut Shared) -> Task<Message> {
    if let Some(viewer) = win.viewer_mut()
        && let Some(session) = viewer.video.session.as_mut()
    {
        if session.playing {
            session.pause();
        } else {
            session.play();
        }
    }
    Task::none()
}

pub(crate) fn seek_drag(win: &mut Window, _shared: &mut Shared, secs: f64) -> Task<Message> {
    if let Some(viewer) = win.viewer_mut()
        && viewer.video.session.is_some()
    {
        viewer.video.seek_drag = Some(secs);
    }
    Task::none()
}

pub(crate) fn seek_release(win: &mut Window, _shared: &mut Shared) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let (Some(target), Some(session)) =
        (viewer.video.seek_drag.take(), viewer.video.session.as_ref())
    else {
        return Task::none();
    };
    viewer.video.session =
        Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
    Task::none()
}

/// Step one frame forward (`dir` +1) or back (-1), pausing. Backward
/// re-seeks, so it is imprecise on variable frame rates.
pub(crate) fn step_frame(win: &mut Window, _shared: &mut Shared, dir: i32) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.session.as_ref() else {
        return Task::none();
    };
    let frame = session
        .frame_duration()
        .unwrap_or(std::time::Duration::from_millis(33));
    let mut target = session.position().as_secs_f64() + frame.as_secs_f64() * f64::from(dir);
    if let Some(duration) = session.duration() {
        target = target.min(duration.as_secs_f64() - 0.5);
    }
    let mut next = session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0)));
    next.pause();
    viewer.video.session = Some(next);
    Task::none()
}

pub(crate) fn seek_by(win: &mut Window, _shared: &mut Shared, delta: f64) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.session.as_ref() else {
        return Task::none();
    };
    let mut target = session.position().as_secs_f64() + delta;
    if let Some(duration) = session.duration() {
        target = target.min(duration.as_secs_f64() - 0.5);
    }
    viewer.video.session =
        Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
    viewer.video.controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    Task::none()
}

/// How long the volume must rest before its config save fires. A slider drag
/// or a held nudge key emits a step per event. Each step reaches the session
/// at once, so only the settled value needs the fsync'd write.
const VOLUME_SAVE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// When the settled volume save is due. Each step pushes it forward, so a burst
/// coalesces into one write: `tick` fires the save once now passes the deadline.
/// Sitting on the update thread (via `tick`), the save then reads the live
/// config, not a clone frozen at arm time.
static VOLUME_SAVE_DUE: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Push the volume save deadline out past the settle window, superseding any
/// pending one so a drag or nudge burst writes only once, after it rests.
fn arm_volume_save() {
    if let Ok(mut due) = VOLUME_SAVE_DUE.lock() {
        *due = Some(std::time::Instant::now() + VOLUME_SAVE_SETTLE);
    }
}

/// Whether the armed volume save is due now, and the deadline to keep. A passed
/// deadline fires once and clears (one write per burst); a future one waits.
fn volume_save_due(
    now: std::time::Instant,
    due: Option<std::time::Instant>,
) -> (bool, Option<std::time::Instant>) {
    match due {
        Some(at) if now >= at => (true, None),
        other => (false, other),
    }
}

/// Fire the settled volume save if its deadline has passed, writing the live
/// config. Called each `tick` while a session runs, so the save lands about one
/// settle window after the last volume change.
fn poll_volume_save(shared: &Shared) -> Task<Message> {
    let ready = {
        let Ok(mut due) = VOLUME_SAVE_DUE.lock() else {
            return Task::none();
        };
        let (ready, keep) = volume_save_due(std::time::Instant::now(), *due);
        *due = keep;
        ready
    };
    if ready {
        Task::future(shared.config.clone().save()).discard()
    } else {
        Task::none()
    }
}

pub(crate) fn set_volume(win: &mut Window, shared: &mut Shared, volume: f32) -> Task<Message> {
    shared.config.standard.video.volume = volume.clamp(0.0, 1.0);
    shared.config.standard.video.muted = false;
    if let Some(viewer) = win.viewer_mut()
        && let Some(session) = viewer.video.session.as_mut()
    {
        session.set_volume(volume);
    }
    arm_volume_save();
    // A paused video's ticks stop once its controls settle, so the deadline
    // needs one wakeup of its own or the save waits for the next playback.
    // Early wakeups from a burst see a pushed-forward deadline and no-op,
    // keeping one write per burst.
    Task::future(async {
        tokio::time::sleep(VOLUME_SAVE_SETTLE + std::time::Duration::from_millis(50)).await;
        Message::VideoControls(VideoMessage::Tick)
    })
}

pub(crate) fn nudge_volume(win: &mut Window, shared: &mut Shared, delta: f32) -> Task<Message> {
    let volume = (shared.config.standard.video.volume + delta).clamp(0.0, 1.0);
    if let Some(viewer) = win.viewer_mut()
        && viewer.video.session.is_some()
    {
        viewer.video.controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    }
    set_volume(win, shared, volume)
}

pub(crate) fn toggle_mute(win: &mut Window, shared: &mut Shared) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.session.as_mut() else {
        return Task::none();
    };
    session.toggle_mute();
    shared.config.standard.video.muted = win
        .viewer()
        .and_then(|v| v.video.session.as_ref())
        .is_some_and(|s| s.muted);
    save_config(win, shared)
}

pub(crate) fn toggle_loop(win: &mut Window, shared: &mut Shared) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.session.as_mut() else {
        return Task::none();
    };
    session.set_looping(!session.looping());
    shared.config.standard.video.looping = win
        .viewer()
        .and_then(|v| v.video.session.as_ref())
        .is_some_and(|s| s.looping());
    save_config(win, shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn set_volume_applies_at_once_and_only_defers_the_save() {
        let mut app = viewing_app(&["a.mp4"], 0);
        let _ = set_volume(&mut app.window, &mut app.shared, 0.3);
        // The session and the in-memory config take the value immediately.
        // Only the disk write is debounced.
        assert_eq!(app.shared.config.standard.video.volume, 0.3);
        assert!(!app.shared.config.standard.video.muted);
    }

    #[test]
    fn a_volume_save_fires_once_after_its_deadline_passes() {
        let now = std::time::Instant::now();
        // A passed deadline fires and clears, so the burst writes exactly once.
        let past = now - std::time::Duration::from_millis(1);
        assert_eq!(volume_save_due(now, Some(past)), (true, None));
        // A deadline still in the future waits, keeping the pending save armed.
        let future = now + std::time::Duration::from_millis(1);
        assert_eq!(volume_save_due(now, Some(future)), (false, Some(future)));
        // Nothing armed is nothing to do.
        assert_eq!(volume_save_due(now, None), (false, None));
    }

    #[test]
    fn set_volume_clamps_into_range() {
        let mut app = viewing_app(&["a.mp4"], 0);
        let _ = set_volume(&mut app.window, &mut app.shared, 1.7);
        assert_eq!(app.shared.config.standard.video.volume, 1.0);
        let _ = set_volume(&mut app.window, &mut app.shared, -0.5);
        assert_eq!(app.shared.config.standard.video.volume, 0.0);
    }

    // Stub sessions report finished with zero frames shown, standing in for
    // a session whose decode setup failed without recording an error.
    #[cfg(not(feature = "video"))]
    #[test]
    fn a_zero_frame_looping_session_never_reopens() {
        let mut app = viewing_app(&["a.mp4"], 0);
        let mut session = crate::video::VideoSession::open(
            PathBuf::from("a.mp4"),
            std::time::Duration::ZERO,
            1.0,
            false,
            true,
            false,
        );
        session.playing = true;
        session.temp = Some(crate::video::TempFileGuard::new(
            std::env::temp_dir().join("scry-video-flow-test.mp4"),
        ));
        app.viewer_mut().unwrap().video.session = Some(session);
        let _ = tick(&mut app.window, &mut app.shared);
        // A reopen would have replaced the session, dropping its temp guard
        // (the stub reopen carries none). The zero-frame guard keeps it.
        let v = app.viewer().unwrap();
        assert!(v.video.session.as_ref().unwrap().temp.is_some());
        assert!(!matches!(v.displayed, DisplayedImage::Error { .. }));
    }

    // Real decode threads: opening a file that does not exist must record a
    // setup failure that tick surfaces as an error and never reopens.
    #[cfg(feature = "video")]
    #[test]
    fn a_failed_open_shows_the_error_and_tears_the_session_down() {
        let missing = std::env::temp_dir().join("scryglass-missing-video-test.mp4");
        let _ = std::fs::remove_file(&missing);
        let mut app = viewing_app(&[missing.to_str().unwrap()], 0);
        app.shared.config.standard.video.looping = true;
        let session = crate::video::VideoSession::open(
            missing.clone(),
            std::time::Duration::ZERO,
            1.0,
            true,
            true,
            false,
        );
        app.viewer_mut().unwrap().video.session = Some(session);
        app.viewer_mut().unwrap().pending_since = Some(Instant::now());
        // Wait for the decode thread to record the setup failure.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app
            .viewer()
            .unwrap()
            .video
            .session
            .as_ref()
            .unwrap()
            .failed()
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "setup failure never reported"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = tick(&mut app.window, &mut app.shared);
        let v = app.viewer().unwrap();
        // The session is gone (not reopened despite looping), the spinner is
        // cleared, and the file shows an error like a broken still would.
        assert!(v.video.session.is_none());
        assert!(v.pending_since.is_none());
        assert!(matches!(v.displayed, DisplayedImage::Error { .. }));
        assert!(v.failed_loads.contains_key(missing.as_path()));
    }

    #[test]
    fn extraction_landing_while_minimized_pauses_and_rearms_decay() {
        let mut app = viewing_app(&["clip.mp4"], 0);
        app.window.minimized = true;
        app.window.focused = false;
        app.shared.config.advanced.resource.minimized.video.pause = true;
        let before = app.window.decay_generation;
        let _ = extracted(
            &mut app.window,
            &mut app.shared,
            PathBuf::from("clip.mp4"),
            Ok(std::env::temp_dir().join("scry-extract-min-test.mp4")),
        );
        let v = app.viewer().unwrap();
        assert!(v.video.session.as_ref().is_some_and(|s| !s.playing));
        // The resume flag and a fresh decay generation mirror what the
        // Minimized handler does for a session that predates the minimize.
        assert!(app.window.video_resumes_on_restore);
        assert_ne!(app.window.decay_generation, before);
    }

    #[test]
    fn extraction_landing_focused_does_not_rearm_decay() {
        let mut app = viewing_app(&["clip.mp4"], 0);
        let before = app.window.decay_generation;
        let _ = extracted(
            &mut app.window,
            &mut app.shared,
            PathBuf::from("clip.mp4"),
            Ok(std::env::temp_dir().join("scry-extract-fg-test.mp4")),
        );
        assert!(app.viewer().unwrap().video.session.is_some());
        assert!(!app.window.video_resumes_on_restore);
        assert_eq!(app.window.decay_generation, before);
    }
}
