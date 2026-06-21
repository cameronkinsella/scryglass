//! Boot: build the initial application state and start any startup work.

use std::path::PathBuf;

use iced::{Size, Task};

use crate::config::AppConfig;
use crate::media::disk_thumbs::DiskThumbs;
use crate::media::pipeline::Pipeline;

use super::state::Session;
use super::{App, Message, recalc_viewport, update};

/// Boot function: creates the initial state. Called once by iced.
///
/// If a file or directory path was passed on the command line (e.g. via
/// "Open with…" in a file manager), opening it starts immediately.
pub fn boot(initial_path: Option<PathBuf>) -> (App, Task<Message>) {
    let config = AppConfig::load();
    let disk_thumbs = DiskThumbs::create(config.disk_thumbs);

    // Startup housekeeping for the persistent thumbnail store: expire
    // long-unused entries and trim to the size cap. Local cache metadata
    // only. Source files (and sleeping drives) are never touched.
    let housekeeping = match disk_thumbs.clone() {
        Some(disk) => Task::future(async move {
            let _ = tokio::task::spawn_blocking(move || disk.housekeep()).await;
        })
        .discard(),
        None => Task::none(),
    };
    // Sweep video extractions orphaned by a crash or hard kill.
    let video_cleanup = Task::future(async {
        let _ = tokio::task::spawn_blocking(crate::video::clean_extraction_dir).await;
    })
    .discard();

    let mut app = App {
        session: Session::Empty,
        config,
        pipeline: Pipeline::new(disk_thumbs),
        open_menu: None,
        viewport_size: Size::new(800.0, 600.0),
        last_cursor_pos: iced::Point::ORIGIN,
        window_size: Size::new(800.0, 600.0),
        context_menu_pos: None,
        fullscreen: false,
        help_open: false,
        modal: None,
        disk_cache_size: None,
        associations_registered: crate::platform::file_associations_registered(),
        opening_since: None,
        toasts: Vec::new(),
        next_toast_id: 0,
    };
    recalc_viewport(&mut app);

    let open = match initial_open_path(initial_path) {
        Some(path) => {
            app.opening_since = Some(iced::time::Instant::now());
            update::open_path(path)
        }
        None => Task::none(),
    };

    (app, Task::batch([housekeeping, video_cleanup, open]))
}

/// The CLI path, if it points to an existing file or directory.
fn initial_open_path(path: Option<PathBuf>) -> Option<PathBuf> {
    let path = path?;
    (path.is_file() || path.is_dir()).then_some(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn initial_open_path_returns_existing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("photo.png");
        fs::write(&file, b"").unwrap();
        assert_eq!(initial_open_path(Some(file.clone())), Some(file));
    }

    #[test]
    fn initial_open_path_returns_existing_directory() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            initial_open_path(Some(dir.path().to_path_buf())),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn initial_open_path_rejects_missing_path() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.png");
        assert_eq!(initial_open_path(Some(missing)), None);
    }

    #[test]
    fn initial_open_path_without_path_returns_none() {
        assert_eq!(initial_open_path(None), None);
    }
}
