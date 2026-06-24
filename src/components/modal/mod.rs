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
        Some(Modal::Rename { input, format }) => {
            let warning = format.and_then(|f| rename_warning(input, f));
            widget::rename_dialog(input, warning.as_deref()).map(AppMessage::Modal)
        }
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
                viewer.failed_loads.remove(&path);

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
            let format = sniff_file_format(&target);
            // Preselect the name but not the extension, so a quick retype keeps
            // the extension. Whole-field select when there's nothing to protect.
            let id = widget::rename_input_id();
            let select = match name_stem_len(&input) {
                Some(len) => iced::widget::operation::select_range(id.clone(), 0, len),
                None => iced::widget::operation::select_all(id.clone()),
            };
            app.modal = Some(Modal::Rename { input, format });
            Task::batch([iced::widget::operation::focus(id), select])
        }

        Message::RenameInput(text) => {
            if let Some(Modal::Rename { input, .. }) = &mut app.modal {
                *input = text;
            }
            Task::none()
        }

        Message::CommitRename => {
            let Some(Modal::Rename { input, .. }) = &app.modal else {
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

        Message::RenameFinished(old, new, result, resume) => match result {
            Err(e) => {
                // The rename failed, so put the torn-down video back as it was.
                if let Some(resume) = resume {
                    resume_video(app, resume, old.clone());
                }
                push_toast(app, ToastKind::Error, format!("Couldn't rename: {e}"))
            }
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
                    viewer.failed_loads.remove(&old);
                    if viewer.displayed_path.as_deref() == Some(&*old) {
                        viewer.displayed_path = Some(new.clone());
                    }
                    if let Some((p, _)) = &mut viewer.exif
                        && *p == old
                    {
                        *p = new.clone();
                    }
                }

                let refresh = rename_refresh(
                    crate::video::is_video(&old),
                    crate::video::is_video(&new),
                    resume.is_some(),
                );
                match refresh {
                    // Still a video: resume it on the new file at its position.
                    RenameRefresh::Resume => {
                        if let Some(resume) = resume {
                            resume_video(app, resume, new);
                        }
                        purge
                    }
                    // The rename crossed the image/video line, so reload the
                    // current file to show or hide the player and its controls.
                    RenameRefresh::Reload => match app.viewer().map(|v| v.nav.cursor()) {
                        Some(cursor) => {
                            Task::batch([purge, complete_navigation(app, cursor, true)])
                        }
                        None => purge,
                    },
                    RenameRefresh::Keep => purge,
                }
            }
        },

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

/// How the view should react to a successful rename.
#[derive(Debug, PartialEq, Eq)]
enum RenameRefresh {
    /// The open video kept a video name. Resume it at its saved position.
    Resume,
    /// The rename crossed the image/video line. Reload to match the new kind.
    Reload,
    /// Same kind on both sides, so the current display already fits.
    Keep,
}

/// Decide how to refresh after a rename. A video that stays a video resumes in
/// place. A rename that flips image to video (or back) reloads so the player
/// and its controls appear or disappear without a manual navigation.
fn rename_refresh(old_is_video: bool, new_is_video: bool, had_open_video: bool) -> RenameRefresh {
    if had_open_video && new_is_video {
        RenameRefresh::Resume
    } else if old_is_video != new_is_video {
        RenameRefresh::Reload
    } else {
        RenameRefresh::Keep
    }
}

/// Where the name ends and the extension begins, so the rename field can
/// preselect just the name. `None` when there's no extension to leave alone.
fn name_stem_len(name: &str) -> Option<usize> {
    let path = Path::new(name);
    path.extension()?;
    Some(path.file_stem()?.to_str()?.chars().count())
}

/// Read a file's leading bytes and sniff its real format for the rename hint.
fn sniff_file_format(path: &Path) -> Option<crate::media::FileFormat> {
    use std::io::Read;
    let mut magic = [0u8; 16];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut magic).ok()?;
    crate::media::sniff_format(&magic[..read])
}

/// A note for the rename dialog when the typed extension would mislabel the
/// file (naming a PNG ".jpg", say). `None` when the extension fits or is absent.
fn rename_warning(name: &str, format: crate::media::FileFormat) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    if format.extensions.contains(&ext.as_str()) {
        return None;
    }
    Some(format!(
        "Saving as .{ext}, but the contents are {}.",
        format.label
    ))
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
            format: None,
        });
        let _ = update(&mut app, Message::RenameInput("new".into()));
        assert!(matches!(&app.modal, Some(Modal::Rename { input, .. }) if input.as_str() == "new"));
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
            matches!(&app.modal, Some(Modal::Rename { input, .. }) if input.as_str() == "photo.png")
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
            format: None,
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
            format: None,
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

    #[test]
    fn rename_refresh_tracks_the_image_video_boundary() {
        // The open video keeps a video name: resume in place at its position.
        assert_eq!(rename_refresh(true, true, true), RenameRefresh::Resume);
        // Crossing the line either way reloads so controls appear or vanish.
        assert_eq!(rename_refresh(false, true, false), RenameRefresh::Reload);
        assert_eq!(rename_refresh(true, false, true), RenameRefresh::Reload);
        // Staying on the same side leaves the current display untouched.
        assert_eq!(rename_refresh(false, false, false), RenameRefresh::Keep);
        assert_eq!(rename_refresh(true, true, false), RenameRefresh::Keep);
    }

    #[test]
    fn stem_selection_stops_before_the_extension() {
        assert_eq!(name_stem_len("photo.png"), Some(5));
        assert_eq!(name_stem_len("my.photo.png"), Some(8));
        assert_eq!(name_stem_len("IMG.JPG"), Some(3));
        // Nothing to protect: no extension, or a leading-dot name.
        assert_eq!(name_stem_len("noext"), None);
        assert_eq!(name_stem_len(".png"), None);
    }

    #[test]
    fn warns_only_when_the_extension_misrepresents_the_file() {
        let png = crate::media::FileFormat {
            label: "PNG",
            extensions: &["png"],
        };
        assert!(rename_warning("photo.jpg", png).unwrap().contains("PNG"));
        assert!(rename_warning("photo.png", png).is_none());
        // Case-insensitive, and a missing extension says nothing.
        assert!(rename_warning("photo.PNG", png).is_none());
        assert!(rename_warning("photo", png).is_none());
        // Every honest extension for the format is accepted.
        let jpeg = crate::media::FileFormat {
            label: "JPEG",
            extensions: &["jpg", "jpeg"],
        };
        assert!(rename_warning("photo.jpeg", jpeg).is_none());
    }
}
