#[derive(Debug, Clone)]
pub enum Message {
    Show,
    Dismiss,
    CopyImage,
    CopyImageFinished(Result<(), String>),
    CopyFile,
    CopyFilePath,
    CopyFilename,
    OpenImageLocation,
    ImageProperties,
}
use iced::{Element, Task};

use crate::app::state::DisplayedImage;
use crate::app::update::{copy_bitmap, push_toast};
use crate::app::{App, ContextMenuMessage, Message as AppMessage, TOOLBAR_HEIGHT};
use crate::components::empty;
use crate::components::toasts::ToastKind;
use crate::media::pipeline::Source;

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    let Some(pos) = app.context_menu_pos else {
        return empty();
    };
    let toolbar_offset = if app.config.show_toolbar && !app.fullscreen {
        TOOLBAR_HEIGHT
    } else {
        0.0
    };
    let adjusted_pos = iced::Point::new(pos.x, pos.y - toolbar_offset);
    let bounds = iced::Size::new(
        app.window_size.width,
        app.window_size.height - toolbar_offset,
    );
    let clamped = widget::clamp_menu_pos(adjusted_pos, widget::MENU_SIZE, bounds);
    let can_modify =
        !app.config.read_only && app.viewer().is_some_and(|v| matches!(v.source, Source::Fs));
    widget::context_menu(clamped, app.config.show_toolbar, can_modify)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::CopyImageFinished(result) => match result {
            Ok(()) => push_toast(app, ToastKind::Info, "Image copied".into()),
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't copy: {e}")),
        },

        Message::Show => {
            app.context_menu_pos = Some(app.last_cursor_pos);
            Task::none()
        }

        Message::Dismiss => {
            app.context_menu_pos = None;
            Task::none()
        }

        Message::CopyImage => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            // Copy the displayed pixels as a real bitmap. It works for any
            // source (archives included) and pastes into image editors.
            let DisplayedImage::Full { allocation, .. } = &viewer.displayed else {
                return push_toast(app, ToastKind::Info, "Image is still loading".into());
            };
            let handle = allocation.handle().clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || copy_bitmap(&handle))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                |result| AppMessage::ContextMenu(ContextMenuMessage::CopyImageFinished(result)),
            )
        }

        Message::CopyFile => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path = viewer.current_disk_path();
            let copy = Task::future(async move {
                crate::platform::copy_file_to_clipboard(&path);
            })
            .discard();
            Task::batch([
                copy,
                push_toast(app, ToastKind::Info, "File copied".to_string()),
            ])
        }

        Message::CopyFilePath => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path_str = match &viewer.source {
                Source::Fs => viewer.nav.current().to_string_lossy().to_string(),
                Source::Archive(index) => format!(
                    "{}/{}",
                    index.archive_path.display(),
                    viewer.nav.current().display()
                ),
            };
            Task::batch([
                iced::clipboard::write(path_str),
                push_toast(app, ToastKind::Info, "Path copied".to_string()),
            ])
        }

        Message::CopyFilename => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let name = viewer
                .nav
                .current()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Task::batch([
                iced::clipboard::write(name),
                push_toast(app, ToastKind::Info, "Filename copied".to_string()),
            ])
        }

        Message::OpenImageLocation => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::reveal_in_file_manager(&viewer.current_disk_path());
            Task::none()
        }

        Message::ImageProperties => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::show_properties(&viewer.current_disk_path());
            Task::none()
        }
    }
}
mod widget;
