use std::path::PathBuf;
use std::sync::Arc;

use crate::media::archive::ArchiveIndex;

#[derive(Debug, Clone)]
pub enum Message {
    FileDropped(PathBuf),
    DirectoryScanned(PathBuf, bool, Result<Vec<PathBuf>, String>),
    ArchiveScanned(PathBuf, Result<Arc<ArchiveIndex>, String>),
    OpenFile,
    FileDialogResult(Option<PathBuf>),
    CloseFile,
    Quit,
}
use iced::Task;
use iced::time::Instant;

use crate::app::state::Session;
use crate::app::update::{open_path, open_viewer, push_toast};
use crate::app::{App, Message as AppMessage, OpenMessage};
use crate::components::toasts::ToastKind;
use crate::config::AppConfig;
use crate::media::pipeline::Source;
use crate::nav::Nav;
pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::FileDropped(path) => {
            app.opening_since = Some(Instant::now());
            open_path(path)
        }

        Message::DirectoryScanned(start_file, opened_dir, Ok(files)) => {
            app.opening_since = None;
            match Nav::new(files, &start_file) {
                Ok(nav) => open_viewer(app, nav, Source::Fs, opened_dir),
                Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't open: {e}")),
            }
        }

        Message::DirectoryScanned(_start_file, _opened_dir, Err(err)) => {
            app.opening_since = None;
            push_toast(app, ToastKind::Error, format!("Couldn't open: {err}"))
        }

        Message::ArchiveScanned(archive_path, Ok(index)) => {
            app.opening_since = None;
            let entries = index.image_entries();
            let start = entries.first().cloned();
            match start.and_then(|s| Nav::new(entries, &s).ok()) {
                Some(nav) => open_viewer(app, nav, Source::Archive(index), true),
                None => push_toast(
                    app,
                    ToastKind::Error,
                    format!(
                        "{}: archive contains no supported images",
                        archive_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                ),
            }
        }

        Message::ArchiveScanned(_archive_path, Err(err)) => {
            app.opening_since = None;
            push_toast(
                app,
                ToastKind::Error,
                format!("Couldn't open archive: {err}"),
            )
        }

        Message::OpenFile => {
            app.open_menu = None;
            let extensions = AppConfig::supported_extensions()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            Task::perform(
                async move {
                    let handle = rfd::AsyncFileDialog::new()
                        .add_filter(
                            "Images",
                            &extensions.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        )
                        .add_filter(
                            "Archives",
                            &["zip", "cbz", "tar", "gz", "tgz", "7z", "cb7", "rar", "cbr"],
                        )
                        .pick_file()
                        .await;
                    handle.map(|h| h.path().to_path_buf())
                },
                |path| AppMessage::Open(OpenMessage::FileDialogResult(path)),
            )
        }

        Message::FileDialogResult(Some(path)) => {
            app.opening_since = Some(Instant::now());
            open_path(path)
        }
        Message::FileDialogResult(None) => Task::none(),

        Message::CloseFile => {
            app.open_menu = None;
            app.session = Session::Empty;
            Task::none()
        }

        Message::Quit => iced::exit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, viewing_app};

    #[test]
    fn close_file_empties_the_session() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app, Message::CloseFile);
        assert!(matches!(app.session, Session::Empty));
    }

    #[test]
    fn cancelled_file_dialog_is_a_noop() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::FileDialogResult(None));
        assert!(app.opening_since.is_none());
    }

    #[test]
    fn open_file_closes_the_menu_and_builds_a_dialog() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::OpenFile);
        assert!(app.open_menu.is_none());
    }

    #[test]
    fn quit_builds_an_exit_task() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::Quit);
    }

    #[tokio::test]
    async fn file_dropped_marks_the_open_as_in_flight() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::FileDropped("x.png".into()));
        assert!(app.opening_since.is_some());
    }

    #[tokio::test]
    async fn picked_file_marks_the_open_as_in_flight() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::FileDialogResult(Some("x.png".into())));
        assert!(app.opening_since.is_some());
    }

    #[tokio::test]
    async fn directory_scanned_opens_a_viewer() {
        let mut app = empty_app();
        app.opening_since = Some(iced::time::Instant::now());
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let _ = update(
            &mut app,
            Message::DirectoryScanned("a.png".into(), true, Ok(files)),
        );
        assert!(app.opening_since.is_none());
        assert!(app.viewer().is_some());
    }

    #[tokio::test]
    async fn directory_scan_error_clears_progress_and_toasts() {
        let mut app = empty_app();
        app.opening_since = Some(iced::time::Instant::now());
        let before = app.toasts.len();
        let _ = update(
            &mut app,
            Message::DirectoryScanned("a.png".into(), true, Err("nope".into())),
        );
        assert!(app.opening_since.is_none());
        assert!(app.toasts.len() > before);
    }

    #[tokio::test]
    async fn archive_scan_error_clears_progress_and_toasts() {
        let mut app = empty_app();
        app.opening_since = Some(iced::time::Instant::now());
        let before = app.toasts.len();
        let _ = update(
            &mut app,
            Message::ArchiveScanned("a.zip".into(), Err("bad".into())),
        );
        assert!(app.opening_since.is_none());
        assert!(app.toasts.len() > before);
    }
}
