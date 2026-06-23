use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Message {
    RequestDelete,
    ConfirmDeleteNow,
    DeleteFinished(PathBuf, Result<(), String>),
    RequestRename,
    RenameInput(String),
    CommitRename,
    RenameFinished(PathBuf, PathBuf, Result<(), String>, Option<VideoResume>),
    Submit,
    Cancel,
}

/// How to resume a video that was torn down so its file could be renamed.
#[derive(Debug, Clone, Copy)]
pub struct VideoResume {
    position: Duration,
    volume: f32,
    muted: bool,
    looping: bool,
    hardware: bool,
    playing: bool,
}

use iced::{Element, Task};

use crate::app::state::Session;
use crate::app::update::{
    complete_navigation, file_op_target, fire_delete, purge_disk_thumb, push_toast, validate_rename,
};
use crate::app::{App, Message as AppMessage, Modal};
use crate::components::empty;
use crate::components::toasts::ToastKind;

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    match &app.modal {
        Some(Modal::ConfirmDelete(path)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            widget::confirm_delete(&name).map(AppMessage::Modal)
        }
        Some(Modal::Rename { input }) => widget::rename_dialog(input).map(AppMessage::Modal),
        _ => empty(),
    }
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::RequestDelete => {
            let target = match file_op_target(app) {
                Ok(target) => target,
                Err(refusal) => return refusal,
            };
            if app.config.confirm_delete {
                app.modal = Some(Modal::ConfirmDelete(target));
                Task::none()
            } else {
                fire_delete(app, target)
            }
        }

        Message::ConfirmDeleteNow => {
            let Some(Modal::ConfirmDelete(path)) = app.modal.take() else {
                return Task::none();
            };
            fire_delete(app, path)
        }

        Message::DeleteFinished(path, result) => match result {
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't delete: {e}")),
            Ok(()) => {
                let purge = purge_disk_thumb(&app.pipeline, &path);
                let Some(viewer) = app.viewer_mut() else {
                    return purge;
                };
                viewer.cache.remove(&path);
                viewer.thumbs.remove(&path);
                viewer.anim_player.remove(&path);
                viewer.failed_thumbs.remove(&path);

                if !viewer.nav.remove(&path) {
                    app.session = Session::Empty;
                    let toast = push_toast(app, ToastKind::Info, "Moved to Recycle Bin".into());
                    return Task::batch([purge, toast]);
                }
                let cursor = viewer.nav.cursor();
                let nav = complete_navigation(app, cursor, true);
                let toast = push_toast(app, ToastKind::Info, "Moved to Recycle Bin".into());
                Task::batch([purge, nav, toast])
            }
        },

        Message::RequestRename => {
            let target = match file_op_target(app) {
                Ok(target) => target,
                Err(refusal) => return refusal,
            };
            let input = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            app.modal = Some(Modal::Rename { input });
            Task::batch([
                iced::widget::operation::focus(widget::rename_input_id()),
                iced::widget::operation::select_all(widget::rename_input_id()),
            ])
        }

        Message::RenameInput(text) => {
            if let Some(Modal::Rename { input }) = &mut app.modal {
                *input = text;
            }
            Task::none()
        }

        Message::CommitRename => {
            let Some(Modal::Rename { input }) = &app.modal else {
                return Task::none();
            };
            let name = match validate_rename(input) {
                Ok(name) => name,
                Err(e) => return push_toast(app, ToastKind::Error, e),
            };
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let old = viewer.nav.current().to_path_buf();
            let new = old.parent().unwrap_or(Path::new("")).join(name);
            app.modal = None;
            if new == old {
                return Task::none();
            }
            if new.exists() {
                return push_toast(
                    app,
                    ToastKind::Error,
                    "a file with that name already exists".to_string(),
                );
            }
            // Renaming the open video means FFmpeg is holding its file handle,
            // so tear the session down (remembering how to resume) first.
            let resume = take_video_for_rename(app, &old);
            let (from, to) = (old.clone(), new.clone());
            Task::perform(rename_with_retry(from, to), move |result| {
                AppMessage::Modal(Message::RenameFinished(old, new, result, resume))
            })
        }

        Message::RenameFinished(old, new, result, resume) => {
            let resume = resume.map(|r| {
                (
                    r,
                    if result.is_ok() {
                        new.clone()
                    } else {
                        old.clone()
                    },
                )
            });
            let task = match result {
                Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't rename: {e}")),
                Ok(()) => {
                    let purge = purge_disk_thumb(&app.pipeline, &old);
                    if let Some(viewer) = app.viewer_mut() {
                        viewer.nav.rename(&old, new.clone());
                        if let Some(image) = viewer.cache.remove(&old) {
                            let cost = image.byte_cost();
                            viewer.cache.insert(new.clone(), image, cost);
                        }
                        if let Some(thumb) = viewer.thumbs.remove(&old) {
                            let cost = thumb.byte_cost();
                            viewer.thumbs.insert(new.clone(), thumb, cost);
                        }
                        viewer.anim_player.remove(&old);
                        if viewer.displayed_path.as_deref() == Some(&*old) {
                            viewer.displayed_path = Some(new.clone());
                        }
                        if let Some((p, _)) = &mut viewer.exif
                            && *p == old
                        {
                            *p = new.clone();
                        }
                    }
                    purge
                }
            };
            // Resume the video on the renamed file, or the original if it failed.
            if let Some((resume, path)) = resume {
                resume_video(app, resume, path);
            }
            task
        }

        Message::Submit => match &app.modal {
            Some(Modal::ConfirmDelete(_)) => update(app, Message::ConfirmDeleteNow),
            Some(Modal::Rename { .. }) => update(app, Message::CommitRename),
            Some(Modal::Settings) => update(app, Message::Cancel),
            None => Task::none(),
        },

        Message::Cancel => {
            app.modal = None;
            Task::none()
        }
    }
}

/// Rename, retrying briefly: the video decoder can keep the old file open for
/// a moment on Windows after its session drops.
async fn rename_with_retry(old: PathBuf, new: PathBuf) -> Result<(), String> {
    let mut err = String::new();
    for attempt in 0..5 {
        match tokio::fs::rename(&old, &new).await {
            Ok(()) => return Ok(()),
            Err(e) => err = e.to_string(),
        }
        if attempt < 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Err(err)
}

/// If `old` is the open video, drop its session (releasing the file handle)
/// and return how to resume it once the file is renamed.
fn take_video_for_rename(app: &mut App, old: &Path) -> Option<VideoResume> {
    let hardware = app.config.hardware_decode;
    let viewer = app.viewer_mut()?;
    let session = viewer.video.as_ref()?;
    if session.path != *old {
        return None;
    }
    let resume = VideoResume {
        position: session.position(),
        volume: session.volume,
        muted: session.muted,
        looping: session.looping,
        hardware,
        playing: session.playing,
    };
    viewer.video = None;
    Some(resume)
}

/// Reopen a torn-down video at `path` and its saved position.
fn resume_video(app: &mut App, resume: VideoResume, path: PathBuf) {
    let Some(viewer) = app.viewer_mut() else {
        return;
    };
    let mut session = crate::video::VideoSession::open(
        path,
        resume.position,
        resume.volume,
        resume.muted,
        resume.looping,
        resume.hardware,
    );
    if !resume.playing {
        session.pause();
    }
    viewer.video = Some(session);
}

mod widget;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, viewing_app};

    #[test]
    fn rename_input_updates_the_dialog_text() {
        let mut app = empty_app();
        app.modal = Some(Modal::Rename {
            input: "old".into(),
        });
        let _ = update(&mut app, Message::RenameInput("new".into()));
        assert!(matches!(&app.modal, Some(Modal::Rename { input }) if input.as_str() == "new"));
    }

    #[test]
    fn cancel_closes_the_modal() {
        let mut app = empty_app();
        app.modal = Some(Modal::Settings);
        let _ = update(&mut app, Message::Cancel);
        assert!(app.modal.is_none());
    }

    #[test]
    fn request_rename_opens_the_dialog_with_the_current_name() {
        let mut app = viewing_app(&["photo.png", "b.png"], 0);
        app.config.read_only = false;
        let _ = update(&mut app, Message::RequestRename);
        assert!(
            matches!(&app.modal, Some(Modal::Rename { input }) if input.as_str() == "photo.png")
        );
    }

    #[test]
    fn request_delete_opens_confirmation_when_enabled() {
        let mut app = viewing_app(&["photo.png"], 0);
        app.config.read_only = false;
        app.config.confirm_delete = true;
        let _ = update(&mut app, Message::RequestDelete);
        assert!(matches!(&app.modal, Some(Modal::ConfirmDelete(p)) if p.ends_with("photo.png")));
    }

    #[test]
    fn submit_on_the_settings_modal_closes_it() {
        let mut app = empty_app();
        app.modal = Some(Modal::Settings);
        let _ = update(&mut app, Message::Submit);
        assert!(app.modal.is_none());
    }

    #[test]
    fn committing_the_same_name_closes_without_renaming() {
        let mut app = viewing_app(&["photo.png", "b.png"], 0);
        app.modal = Some(Modal::Rename {
            input: "photo.png".into(),
        });
        let _ = update(&mut app, Message::CommitRename);
        // The dialog closes, and an unchanged name is a no-op (no rename task).
        assert!(app.modal.is_none());
        assert_eq!(
            app.viewer().unwrap().nav.current().to_string_lossy(),
            "photo.png"
        );
    }

    #[test]
    fn committing_a_new_name_closes_the_dialog() {
        let mut app = viewing_app(&["photo.png", "b.png"], 0);
        app.modal = Some(Modal::Rename {
            input: "renamed.png".into(),
        });
        let _ = update(&mut app, Message::CommitRename);
        assert!(app.modal.is_none());
    }

    #[test]
    fn request_delete_without_confirmation_skips_the_modal() {
        let mut app = viewing_app(&["a.png"], 0);
        app.config.read_only = false;
        app.config.confirm_delete = false;
        let _ = update(&mut app, Message::RequestDelete);
        assert!(app.modal.is_none());
    }

    #[test]
    fn submit_on_confirm_delete_clears_the_modal() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.modal = Some(Modal::ConfirmDelete("a.png".into()));
        let _ = update(&mut app, Message::Submit);
        assert!(app.modal.is_none());
    }

    #[tokio::test]
    async fn delete_finished_error_raises_a_toast() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let before = app.toasts.len();
        let _ = update(
            &mut app,
            Message::DeleteFinished("a.png".into(), Err("nope".into())),
        );
        assert!(app.toasts.len() > before);
    }

    #[tokio::test]
    async fn delete_finished_advances_to_the_survivor() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::DeleteFinished("a.png".into(), Ok(())));
        assert_eq!(
            app.viewer().unwrap().nav.current().to_string_lossy(),
            "b.png"
        );
    }

    #[tokio::test]
    async fn deleting_the_last_file_empties_the_session() {
        let mut app = viewing_app(&["only.png"], 0);
        let _ = update(&mut app, Message::DeleteFinished("only.png".into(), Ok(())));
        assert!(matches!(app.session, Session::Empty));
    }

    #[test]
    fn rename_finished_updates_the_navigation_entry() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(
            &mut app,
            Message::RenameFinished("a.png".into(), "renamed.png".into(), Ok(()), None),
        );
        assert_eq!(
            app.viewer().unwrap().nav.current().to_string_lossy(),
            "renamed.png"
        );
    }

    #[test]
    fn renaming_the_open_video_tears_it_down() {
        let mut app = viewing_app(&["clip.mp4"], 0);
        let path = app.viewer().unwrap().nav.current().to_path_buf();
        let open = || {
            crate::video::VideoSession::open(path.clone(), Duration::ZERO, 0.4, false, true, false)
        };
        app.viewer_mut().unwrap().video = Some(open());
        // Renaming the open video releases its session so the file unlocks.
        assert!(take_video_for_rename(&mut app, &path).is_some());
        assert!(app.viewer().unwrap().video.is_none());
        // A different file leaves the video alone.
        app.viewer_mut().unwrap().video = Some(open());
        assert!(take_video_for_rename(&mut app, Path::new("other.mp4")).is_none());
        assert!(app.viewer().unwrap().video.is_some());
    }

    #[tokio::test]
    async fn rename_finished_error_raises_a_toast() {
        let mut app = viewing_app(&["a.png"], 0);
        let before = app.toasts.len();
        let _ = update(
            &mut app,
            Message::RenameFinished("a.png".into(), "b.png".into(), Err("nope".into()), None),
        );
        assert!(app.toasts.len() > before);
    }
}
