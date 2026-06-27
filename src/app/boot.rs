//! Boot: build the initial application state and start any startup work.

use std::collections::HashMap;
use std::path::PathBuf;

use iced::{Point, Size, Task, window};

use crate::config::AppConfig;
use crate::media::disk_thumbs::DiskThumbs;
use crate::media::pipeline::Pipeline;

use super::state::Session;
use super::{App, Envelope, Shared, Window, recalc_viewport, update};

/// Boot function: builds the shared state and opens the first window. Called
/// once by the daemon.
///
/// If a file or directory path was passed on the command line (e.g. via
/// "Open with…" in a file manager), opening it starts immediately.
pub fn boot(initial_path: Option<PathBuf>) -> (App, Task<Envelope>) {
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

    let shared = Shared {
        config,
        pipeline: Pipeline::new(disk_thumbs),
        disk_cache_size: None,
        associations_registered: crate::platform::file_associations_registered(),
        #[cfg(feature = "update-check")]
        update_status: None,
    };

    let (id, opened) = window::open(window_settings(&shared.config));
    let mut win = new_window(id, &shared.config);
    recalc_viewport(&mut win, &shared);

    let open = match initial_open_path(initial_path) {
        Some(path) => {
            win.opening_since = Some(iced::time::Instant::now());
            Envelope::wrap(id, update::open_path(path))
        }
        None => Task::none(),
    };

    let mut windows = HashMap::new();
    windows.insert(id, win);
    let app = App { shared, windows };
    (
        app,
        Task::batch([
            housekeeping,
            video_cleanup,
            opened.map(Envelope::Opened),
            open,
        ]),
    )
}

/// A fresh empty window state for window `id`, seeded from the saved geometry.
pub(crate) fn new_window(id: window::Id, config: &AppConfig) -> Window {
    let size = Size::new(config.window_width, config.window_height);
    let window_pos = match (config.window_x, config.window_y) {
        (Some(x), Some(y)) => Point::new(x, y),
        _ => Point::ORIGIN,
    };
    Window {
        id,
        session: Session::Empty,
        open_menu: None,
        viewport_size: size,
        last_cursor_pos: Point::ORIGIN,
        window_size: size,
        window_pos,
        maximized: config.window_maximized,
        restored_size: size,
        restored_pos: window_pos,
        context_menu_pos: None,
        zoom_slider_open: false,
        fullscreen: config.window_fullscreen,
        focused: true,
        tier_generation: 0,
        minimized: false,
        video_resumes_on_restore: false,
        help_open: false,
        modal: None,
        opening_since: None,
        toasts: Vec::new(),
        next_toast_id: 0,
    }
}

/// Settings for a newly opened viewer window: the saved size and position (the
/// OS places it when there is no saved position yet).
pub(crate) fn window_settings(config: &AppConfig) -> window::Settings {
    let position = match (config.window_x, config.window_y) {
        (Some(x), Some(y)) => window::Position::Specific(Point::new(x, y)),
        _ => window::Position::Default,
    };
    window::Settings {
        size: Size::new(config.window_width, config.window_height),
        position,
        min_size: Some(Size::new(480.0, 420.0)),
        icon: window_icon(),
        // Close requests route through update() so config saves first.
        exit_on_close_request: false,
        ..Default::default()
    }
}

/// Replay a saved maximized/fullscreen state onto a freshly opened window, in
/// order, so the OS rebuilds the same restore stack: exit fullscreen to
/// maximized, then to the restored windowed geometry.
pub(crate) fn replay_window_state(
    id: window::Id,
    maximized: bool,
    fullscreen: bool,
) -> Task<Envelope> {
    let mut task = Task::none();
    if maximized {
        task = task.chain(window::maximize(id, true));
    }
    if fullscreen {
        task = task.chain(window::set_mode(id, window::Mode::Fullscreen));
    }
    task
}

/// Decode the embedded window icon with the image crate. (iced's
/// encoded-bytes icon API needs its codec feature, which is off.)
fn window_icon() -> Option<window::Icon> {
    let img = image::load_from_memory(include_bytes!("../../assets/icon.png"))
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    window::icon::from_rgba(img.into_raw(), w, h).ok()
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
