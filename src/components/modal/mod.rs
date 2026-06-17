use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Message {
    RequestDelete,
    ConfirmDeleteNow,
    DeleteFinished(PathBuf, Result<(), String>),
    RequestRename,
    RenameInput(String),
    CommitRename,
    RenameFinished(PathBuf, PathBuf, Result<(), String>),
    Submit,
    Cancel,
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
            let new = old.parent().unwrap_or(std::path::Path::new("")).join(name);
            app.modal = None;
            if new == old {
                return Task::none();
            }

            Task::perform(
                async move {
                    if tokio::fs::try_exists(&new).await.unwrap_or(false) {
                        let err = "a file with that name already exists".to_string();
                        return (old, new, Err(err));
                    }
                    let result = tokio::fs::rename(&old, &new)
                        .await
                        .map_err(|e| e.to_string());
                    (old, new, result)
                },
                |(old, new, result)| AppMessage::Modal(Message::RenameFinished(old, new, result)),
            )
        }

        Message::RenameFinished(old, new, result) => match result {
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't rename: {e}")),
            Ok(()) => {
                let purge = purge_disk_thumb(&app.pipeline, &old);
                let Some(viewer) = app.viewer_mut() else {
                    return purge;
                };
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
                    *p = new;
                }
                purge
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
mod widget;
