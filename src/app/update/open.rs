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
