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
use crate::app::{ContextMenuMessage, Message as AppMessage, Shared, TOOLBAR_HEIGHT, Window};
use crate::components::empty;
use crate::components::toasts::ToastKind;
use crate::media::pipeline::Source;

pub(crate) fn view<'a>(win: &'a Window, shared: &'a Shared) -> Element<'a, AppMessage> {
    let Some(pos) = win.context_menu_pos else {
        return empty();
    };
    let toolbar_offset = if shared.config.show_toolbar && !win.fullscreen {
        TOOLBAR_HEIGHT
    } else {
        0.0
    };
    let adjusted_pos = iced::Point::new(pos.x, pos.y - toolbar_offset);
    let bounds = iced::Size::new(
        win.window_size.width,
        win.window_size.height - toolbar_offset,
    );
    let can_modify =
        !shared.config.read_only && win.viewer().is_some_and(|v| matches!(v.source, Source::Fs));
    let placed = widget::flip_menu_pos(adjusted_pos, widget::menu_size(can_modify), bounds);
    widget::context_menu(placed, shared.config.show_toolbar, can_modify)
}

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::CopyImageFinished(result) => match result {
            Ok(()) => push_toast(win, shared, ToastKind::Info, "Image copied".into()),
            Err(e) => push_toast(win, shared, ToastKind::Error, format!("Couldn't copy: {e}")),
        },

        Message::Show => {
            win.context_menu_pos = Some(win.last_cursor_pos);
            Task::none()
        }

        Message::Dismiss => {
            win.context_menu_pos = None;
            Task::none()
        }

        Message::CopyImage => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
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
                return push_toast(
                    win,
                    shared,
                    ToastKind::Info,
                    "Image is still loading".into(),
                );
            };
            Task::perform(
                async move { task.await.map_err(|e| e.to_string()).and_then(|r| r) },
                |result| AppMessage::ContextMenu(ContextMenuMessage::CopyImageFinished(result)),
            )
        }

        Message::CopyFile => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
                return Task::none();
            };
            let path = viewer.current_disk_path();
            let copy = Task::future(async move {
                crate::platform::copy_file_to_clipboard(&path);
            })
            .discard();
            Task::batch([
                copy,
                push_toast(win, shared, ToastKind::Info, "File copied".to_string()),
            ])
        }

        Message::CopyFilePath => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
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
                push_toast(win, shared, ToastKind::Info, "Path copied".to_string()),
            ])
        }

        Message::CopyFilename => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
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
                push_toast(win, shared, ToastKind::Info, "Filename copied".to_string()),
            ])
        }

        Message::OpenImageLocation => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
                return Task::none();
            };
            crate::platform::reveal_in_file_manager(&viewer.current_disk_path());
            Task::none()
        }

        Message::ImageProperties => {
            win.context_menu_pos = None;
            let Some(viewer) = win.viewer() else {
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
        app.window.last_cursor_pos = iced::Point::new(12.0, 34.0);
        let _ = update(&mut app.window, &mut app.shared, Message::Show);
        assert!(app.window.context_menu_pos == Some(iced::Point::new(12.0, 34.0)));
    }

    #[test]
    fn dismiss_hides_the_menu() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::Dismiss);
        assert!(app.window.context_menu_pos.is_none());
    }

    // push_toast schedules a tokio timer, so this needs a runtime in scope.
    #[tokio::test]
    async fn copy_image_while_loading_reports_it_and_closes_the_menu() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::CopyImage);
        assert!(app.window.context_menu_pos.is_none());
        assert_eq!(app.window.toasts.len(), 1);
    }

    #[tokio::test]
    async fn copy_image_finished_toasts_on_success_and_failure() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::CopyImageFinished(Ok(())),
        );
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::CopyImageFinished(Err("nope".into())),
        );
        assert_eq!(app.window.toasts.len(), 2);
    }

    #[tokio::test]
    async fn copy_file_path_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::CopyFilePath);
        assert!(app.window.context_menu_pos.is_none());
        assert!(!app.window.toasts.is_empty());
    }

    #[tokio::test]
    async fn copy_filename_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::CopyFilename);
        assert!(app.window.context_menu_pos.is_none());
        assert!(!app.window.toasts.is_empty());
    }

    #[tokio::test]
    async fn copy_file_closes_the_menu_and_toasts() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::CopyFile);
        assert!(app.window.context_menu_pos.is_none());
        assert!(!app.window.toasts.is_empty());
    }
}
