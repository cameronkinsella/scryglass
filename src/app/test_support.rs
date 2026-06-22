//! Headless `App` builders for tests: no GPU, no disk store, an in-memory
//! `Nav`. The update layer holds no GPU state, so these are enough to drive
//! messages through `update` and assert state transitions.

use std::path::PathBuf;

use iced::Size;
use iced::widget::image::Handle;

use crate::anim::AnimPlayer;
use crate::app::state::{Session, Thumb, Viewer};
use crate::config::AppConfig;
use crate::media::pipeline::{Pipeline, Source};
use crate::nav::Nav;

use super::App;

/// A headless App with an empty session and default config.
pub(crate) fn empty_app() -> App {
    App {
        session: Session::Empty,
        config: AppConfig::default(),
        pipeline: Pipeline::new(None),
        open_menu: None,
        viewport_size: Size::new(800.0, 600.0),
        last_cursor_pos: iced::Point::ORIGIN,
        window_size: Size::new(800.0, 600.0),
        context_menu_pos: None,
        zoom_slider_open: false,
        fullscreen: false,
        help_open: false,
        modal: None,
        disk_cache_size: None,
        associations_registered: false,
        opening_since: None,
        toasts: Vec::new(),
        next_toast_id: 0,
    }
}

/// A headless App viewing the given file names, cursor on `cursor`.
pub(crate) fn viewing_app(names: &[&str], cursor: usize) -> App {
    let files: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    let start = files[cursor].clone();
    let nav = Nav::new(files, &start).unwrap();
    let budget = AppConfig::default().cache_budget_mb * 1024 * 1024;
    let viewer = Viewer::new(nav, Source::Fs, AnimPlayer::new(), budget);
    let mut app = empty_app();
    app.session = Session::Viewing(Box::new(viewer));
    app
}

/// A small RGBA thumbnail built from a CPU `Handle` (no GPU upload).
pub(crate) fn thumb(w: u32, h: u32) -> Thumb {
    let handle = Handle::from_rgba(w, h, vec![0u8; (w * h * 4) as usize]);
    Thumb {
        handle,
        size: (w, h),
        original_size: (w, h),
    }
}

/// Give `path` a cached thumbnail, so the viewer treats it as displayable
/// (a blur is on hand) without any GPU upload.
pub(crate) fn cache_thumb(app: &mut App, path: &str, w: u32, h: u32) {
    if let Some(viewer) = app.viewer_mut() {
        let thumb = thumb(w, h);
        let cost = thumb.byte_cost();
        viewer.thumbs.insert(PathBuf::from(path), thumb, cost);
    }
}
