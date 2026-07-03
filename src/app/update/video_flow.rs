use std::path::PathBuf;
use std::sync::Arc;

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
    let target = if crate::app::viewer_math::controls_visible(
        playing,
        viewer.video.seek_drag.is_some(),
        controls_alive,
    ) {
        1.0
    } else {
        0.0
    };
    viewer.video.controls_opacity = crate::app::viewer_math::ease_toward(
        viewer.video.controls_opacity,
        target,
        crate::app::CONTROLS_FADE_STEP,
    );

    let Some(session) = viewer.video.session.as_mut() else {
        return Task::none();
    };

    let Some(frame) = session.poll() else {
        // Only a session with nothing left to show is finished, since
        // queued frames still drain through poll() above.
        if session.finished() {
            if session.looping() {
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
            viewer.video.session = Some(session);
            // The entry has no real path of its own, so grab its thumbnail from
            // the file extracted for playback (none otherwise).
            fire_archive_video_thumb(&shared.pipeline, &shared.thumbs, viewer, entry, temp, guard)
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

pub(crate) fn set_volume(win: &mut Window, shared: &mut Shared, volume: f32) -> Task<Message> {
    shared.config.standard.video.volume = volume.clamp(0.0, 1.0);
    shared.config.standard.video.muted = false;
    if let Some(viewer) = win.viewer_mut()
        && let Some(session) = viewer.video.session.as_mut()
    {
        session.set_volume(volume);
    }
    save_config(win, shared)
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
