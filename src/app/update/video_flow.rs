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

use super::push_toast;
use super::settings::save_config;
use crate::app::App;

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
    viewer.video_controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    match &viewer.source {
        Source::Fs => {
            viewer.video = Some(crate::video::VideoSession::open(
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
            if viewer.video_extracting.as_deref() == Some(&*current) {
                return Task::none();
            }
            viewer.video_extracting = Some(current.clone());
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

pub(crate) fn tick(app: &mut App) -> Task<Message> {
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.as_mut() else {
        return Task::none();
    };

    let Some(frame) = session.poll() else {
        // Only a session with nothing left to show is finished, since
        // queued frames still drain through poll() above.
        if session.finished() {
            if session.looping {
                viewer.video = Some(session.reopen_at(std::time::Duration::ZERO));
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
    viewer.video_frame = Some(Arc::new(frame));
    viewer.displayed = DisplayedImage::Video {
        original_size: (w, h),
    };
    viewer.displayed_path = Some(path);
    viewer.pending_since = None;
    Task::none()
}

pub(crate) fn extracted(
    app: &mut App,
    entry: PathBuf,
    result: Result<PathBuf, String>,
) -> Task<Message> {
    let video_volume = app.config.video_volume;
    let video_muted = app.config.video_muted;
    let video_loop = app.config.video_loop;
    let hardware = app.config.hardware_decode;
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    if viewer.video_extracting.as_deref() == Some(&*entry) {
        viewer.video_extracting = None;
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
            push_toast(app, ToastKind::Error, format!("Couldn't play video: {e}"))
        }
        Ok(temp) => {
            let mut session = crate::video::VideoSession::open(
                temp.clone(),
                std::time::Duration::ZERO,
                video_volume,
                video_muted,
                video_loop,
                hardware,
            );
            session.temp = Some(crate::video::TempFileGuard::new(temp));
            viewer.video = Some(session);
            Task::none()
        }
    }
}

pub(crate) fn play_pause(app: &mut App) -> Task<Message> {
    if let Some(viewer) = app.viewer_mut()
        && let Some(session) = viewer.video.as_mut()
    {
        if session.playing {
            session.pause();
        } else {
            session.play();
        }
    }
    Task::none()
}

pub(crate) fn seek_drag(app: &mut App, secs: f64) -> Task<Message> {
    if let Some(viewer) = app.viewer_mut()
        && viewer.video.is_some()
    {
        viewer.video_seek_drag = Some(secs);
    }
    Task::none()
}

pub(crate) fn seek_release(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let (Some(target), Some(session)) = (viewer.video_seek_drag.take(), viewer.video.as_ref())
    else {
        return Task::none();
    };
    viewer.video = Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
    Task::none()
}

/// Step one frame forward (`dir` +1) or back (-1), pausing. Backward
/// re-seeks, so it is imprecise on variable frame rates.
pub(crate) fn step_frame(app: &mut App, dir: i32) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.as_ref() else {
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
    viewer.video = Some(next);
    Task::none()
}

pub(crate) fn seek_by(app: &mut App, delta: f64) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.as_ref() else {
        return Task::none();
    };
    let mut target = session.position().as_secs_f64() + delta;
    if let Some(duration) = session.duration() {
        target = target.min(duration.as_secs_f64() - 0.5);
    }
    viewer.video = Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
    viewer.video_controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    Task::none()
}

pub(crate) fn set_volume(app: &mut App, volume: f32) -> Task<Message> {
    app.config.video_volume = volume.clamp(0.0, 1.0);
    app.config.video_muted = false;
    if let Some(viewer) = app.viewer_mut()
        && let Some(session) = viewer.video.as_mut()
    {
        session.set_volume(volume);
    }
    save_config(app)
}

pub(crate) fn nudge_volume(app: &mut App, delta: f32) -> Task<Message> {
    let volume = (app.config.video_volume + delta).clamp(0.0, 1.0);
    if let Some(viewer) = app.viewer_mut()
        && viewer.video.is_some()
    {
        viewer.video_controls_until = Some(Instant::now() + VIDEO_CONTROLS_TIMEOUT);
    }
    set_volume(app, volume)
}

pub(crate) fn toggle_mute(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.as_mut() else {
        return Task::none();
    };
    session.toggle_mute();
    app.config.video_muted = app
        .viewer()
        .and_then(|v| v.video.as_ref())
        .is_some_and(|s| s.muted);
    save_config(app)
}

pub(crate) fn toggle_loop(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(session) = viewer.video.as_mut() else {
        return Task::none();
    };
    session.looping = !session.looping;
    app.config.video_loop = app
        .viewer()
        .and_then(|v| v.video.as_ref())
        .is_some_and(|s| s.looping);
    save_config(app)
}
