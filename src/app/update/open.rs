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
    /// The open folder changed on disk. Trigger a re-scan.
    DirectoryChanged(PathBuf),
    /// A re-scan finished. Reconcile the file list with it.
    DirectoryRescanned(PathBuf, Option<Vec<PathBuf>>),
}
use iced::Task;
use iced::time::Instant;

use crate::app::state::Session;
use crate::app::update::media::replace_files_keeping_pending;
use crate::app::update::{complete_navigation, open_path, open_viewer, push_toast};
use crate::app::{Message as AppMessage, OpenMessage, Shared, Window};
use crate::components::toasts::ToastKind;
use crate::config::AppConfig;
use crate::media::pipeline::Source;
use crate::nav::Nav;
pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::FileDropped(path) => {
            win.opening_since = Some(Instant::now());
            open_path(path)
        }

        Message::DirectoryScanned(start_file, opened_dir, Ok(files)) => {
            win.opening_since = None;
            match Nav::new(files, &start_file) {
                Ok(nav) => open_viewer(win, shared, nav, Source::Fs, opened_dir),
                Err(e) => push_toast(win, shared, ToastKind::Error, format!("Couldn't open: {e}")),
            }
        }

        Message::DirectoryScanned(_start_file, _opened_dir, Err(err)) => {
            win.opening_since = None;
            push_toast(
                win,
                shared,
                ToastKind::Error,
                format!("Couldn't open: {err}"),
            )
        }

        Message::ArchiveScanned(archive_path, Ok(index)) => {
            win.opening_since = None;
            let entries = index.image_entries();
            let start = entries.first().cloned();
            match start.and_then(|s| Nav::new(entries, &s).ok()) {
                Some(nav) => open_viewer(win, shared, nav, Source::Archive(index), true),
                None => push_toast(
                    win,
                    shared,
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
            win.opening_since = None;
            push_toast(
                win,
                shared,
                ToastKind::Error,
                format!("Couldn't open archive: {err}"),
            )
        }

        Message::OpenFile => {
            win.open_menu = None;
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
            win.opening_since = Some(Instant::now());
            open_path(path)
        }
        Message::FileDialogResult(None) => Task::none(),

        Message::CloseFile => {
            win.open_menu = None;
            win.session = Session::Empty;
            Task::none()
        }

        // Close this window, saving its state. The process exits when the
        // last window closes (see the daemon's Closed handler).
        Message::Quit => {
            let id = win.id;
            let config = shared.config.clone();
            Task::future(config.save()).then(move |_| iced::window::close(id))
        }

        Message::DirectoryChanged(dir) => Task::perform(
            async move {
                let files = crate::nav::scan_directory(&dir).ok();
                (dir, files)
            },
            |(dir, files)| AppMessage::Open(OpenMessage::DirectoryRescanned(dir, files)),
        ),

        Message::DirectoryRescanned(dir, files) => {
            let Some(files) = files else {
                return Task::none();
            };
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            // Ignore a scan that arrives after navigating to another folder.
            if !matches!(viewer.source, Source::Fs)
                || viewer.nav.current().parent() != Some(dir.as_path())
            {
                return Task::none();
            }
            if files.is_empty() {
                // Every image in the folder is gone: nothing left to show.
                win.session = Session::Empty;
                return Task::none();
            }
            let previous = viewer.nav.current().to_path_buf();
            replace_files_keeping_pending(viewer, files);
            // A file may have been fixed on disk, so drop remembered errors and
            // let the next visit decode afresh.
            viewer.failed_loads.clear();
            if viewer.nav.current() != previous {
                // The on-screen file was deleted externally and the cursor fell
                // back to the start of the new listing. Move the display onto
                // it too, or the deleted image stays on screen while the footer
                // and filmstrip track the new cursor.
                let cursor = viewer.nav.cursor();
                return complete_navigation(win, shared, cursor, true);
            }
            Task::none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, viewing_app};

    #[test]
    fn close_file_empties_the_session() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::CloseFile);
        assert!(matches!(app.window.session, Session::Empty));
    }

    #[test]
    fn cancelled_file_dialog_is_a_noop() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::FileDialogResult(None),
        );
        assert!(app.window.opening_since.is_none());
    }

    #[test]
    fn open_file_closes_the_menu_and_builds_a_dialog() {
        let mut app = empty_app();
        let _ = update(&mut app.window, &mut app.shared, Message::OpenFile);
        assert!(app.window.open_menu.is_none());
    }

    #[test]
    fn quit_builds_an_exit_task() {
        let mut app = empty_app();
        let _ = update(&mut app.window, &mut app.shared, Message::Quit);
    }

    #[tokio::test]
    async fn file_dropped_marks_the_open_as_in_flight() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::FileDropped("x.png".into()),
        );
        assert!(app.window.opening_since.is_some());
    }

    #[tokio::test]
    async fn picked_file_marks_the_open_as_in_flight() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::FileDialogResult(Some("x.png".into())),
        );
        assert!(app.window.opening_since.is_some());
    }

    #[test]
    fn rescan_that_drops_the_current_file_moves_the_display() {
        use crate::app::state::DisplayedImage;

        // Viewing b.png, then an external delete removes it from the listing.
        let mut app = viewing_app(&["a.png", "b.png"], 1);
        {
            let v = app.window.viewer_mut().unwrap();
            v.displayed = DisplayedImage::Full {
                original_size: (2, 2),
                rotated: None,
            };
            v.displayed_path = Some(PathBuf::from("b.png"));
        }
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryRescanned(PathBuf::from(""), Some(vec![PathBuf::from("a.png")])),
        );
        let v = app.viewer().unwrap();
        // The cursor fell back and the display followed it, so the deleted
        // file is no longer what the image area refers to.
        assert_eq!(v.nav.current(), std::path::Path::new("a.png"));
        assert_ne!(
            v.displayed_path.as_deref(),
            Some(std::path::Path::new("b.png"))
        );
        assert!(!matches!(v.displayed, DisplayedImage::Full { .. }));
    }

    #[test]
    fn rescan_with_no_files_left_empties_the_session() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryRescanned(PathBuf::from(""), Some(vec![])),
        );
        assert!(matches!(app.window.session, Session::Empty));
    }

    #[test]
    fn rescan_remaps_a_pending_move_by_file() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        // Aimed at c.png, which the rescan moves to index 0.
        app.window.viewer_mut().unwrap().pending_nav = Some(2);
        let files = vec![
            PathBuf::from("c.png"),
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
        ];
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryRescanned(PathBuf::from(""), Some(files)),
        );
        assert_eq!(app.viewer().unwrap().pending_nav, Some(0));
    }

    #[test]
    fn directory_rescanned_reconciles_the_file_list() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        // The synthetic current file's parent is the empty path.
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryRescanned(PathBuf::from(""), Some(files)),
        );
        assert_eq!(app.viewer().unwrap().nav.files().len(), 3);
        // A scan for a different directory is ignored.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryRescanned(PathBuf::from("elsewhere"), Some(vec![PathBuf::from("z")])),
        );
        assert_eq!(app.viewer().unwrap().nav.files().len(), 3);
    }

    #[tokio::test]
    async fn directory_scanned_opens_a_viewer() {
        let mut app = empty_app();
        app.window.opening_since = Some(iced::time::Instant::now());
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryScanned("a.png".into(), true, Ok(files)),
        );
        assert!(app.window.opening_since.is_none());
        assert!(app.viewer().is_some());
    }

    #[tokio::test]
    async fn directory_scan_error_clears_progress_and_toasts() {
        let mut app = empty_app();
        app.window.opening_since = Some(iced::time::Instant::now());
        let before = app.window.toasts.len();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DirectoryScanned("a.png".into(), true, Err("nope".into())),
        );
        assert!(app.window.opening_since.is_none());
        assert!(app.window.toasts.len() > before);
    }

    #[tokio::test]
    async fn archive_scan_error_clears_progress_and_toasts() {
        let mut app = empty_app();
        app.window.opening_since = Some(iced::time::Instant::now());
        let before = app.window.toasts.len();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ArchiveScanned("a.zip".into(), Err("bad".into())),
        );
        assert!(app.window.opening_since.is_none());
        assert!(app.window.toasts.len() > before);
    }
}
