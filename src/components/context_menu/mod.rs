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
use crate::app::update::{copy_bitmap, copy_rgba_bitmap, push_toast};
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
            // Copy the displayed pixels as a real bitmap (works for any
            // source). Video grabs its current frame off the UI thread.
            let task = match &viewer.displayed {
                DisplayedImage::Full { allocation, .. } => {
                    let handle = allocation.handle().clone();
                    Some(tokio::task::spawn_blocking(move || copy_bitmap(&handle)))
                }
                DisplayedImage::Video { .. } => viewer.video_frame.clone().map(|frame| {
                    tokio::task::spawn_blocking(move || {
                        let (w, h, rgba) = frame.to_rgba();
                        copy_rgba_bitmap(w, h, rgba)
                    })
                }),
                _ => None,
            };
            let Some(task) = task else {
                return push_toast(app, ToastKind::Info, "Image is still loading".into());
            };
            Task::perform(
                async move { task.await.map_err(|e| e.to_string()).and_then(|r| r) },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn show_places_the_menu_at_the_cursor() {
        let mut app = viewing_app(&["a.png"], 0);
        app.last_cursor_pos = iced::Point::new(12.0, 34.0);
        let _ = update(&mut app, Message::Show);
        assert!(app.context_menu_pos == Some(iced::Point::new(12.0, 34.0)));
    }

    #[test]
    fn dismiss_hides_the_menu() {
        let mut app = viewing_app(&["a.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::Dismiss);
        assert!(app.context_menu_pos.is_none());
    }

    // push_toast schedules a tokio timer, so this needs a runtime in scope.
    #[tokio::test]
    async fn copy_image_while_loading_reports_it_and_closes_the_menu() {
        let mut app = viewing_app(&["a.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::CopyImage);
        assert!(app.context_menu_pos.is_none());
        assert_eq!(app.toasts.len(), 1);
    }

    #[tokio::test]
    async fn copy_image_finished_toasts_on_success_and_failure() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app, Message::CopyImageFinished(Ok(())));
        let _ = update(&mut app, Message::CopyImageFinished(Err("nope".into())));
        assert_eq!(app.toasts.len(), 2);
    }

    #[tokio::test]
    async fn copy_file_path_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::CopyFilePath);
        assert!(app.context_menu_pos.is_none());
        assert!(!app.toasts.is_empty());
    }

    #[tokio::test]
    async fn copy_filename_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::CopyFilename);
        assert!(app.context_menu_pos.is_none());
        assert!(!app.toasts.is_empty());
    }

    #[tokio::test]
    async fn copy_file_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::CopyFile);
        assert!(app.context_menu_pos.is_none());
        assert!(!app.toasts.is_empty());
    }
}
