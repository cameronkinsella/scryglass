//! Headless `App` builders for tests: no GPU, no disk store, an in-memory
//! `Nav`. The update layer holds no GPU state, so these are enough to drive
//! messages through `update` and assert state transitions.

use std::collections::HashMap;
use std::path::PathBuf;

use iced::widget::image::Handle;
use iced::{Size, window};

use crate::anim::AnimPlayer;
use crate::app::state::{Session, Thumb, Viewer};
use crate::config::AppConfig;
use crate::media::pipeline::{Pipeline, Source, thumb_key};
use crate::nav::Nav;

use super::{App, Shared, Window};

/// A headless single-window app for tests, holding the per-window [`Window`]
/// and the [`Shared`] state directly so component tests can drive an update
/// fn with `&mut app.window, &mut app.shared` and assert on either half. Lift
/// it into a real multi-window [`App`] with [`into_app`] for the few tests
/// that exercise the top-level update/view.
pub(crate) struct TestApp {
    pub(crate) window: Window,
    pub(crate) shared: Shared,
}

impl TestApp {
    pub(crate) fn viewer(&self) -> Option<&Viewer> {
        self.window.viewer()
    }

    pub(crate) fn viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.window.viewer_mut()
    }
}

/// A headless app with an empty session and default config.
pub(crate) fn empty_app() -> TestApp {
    TestApp {
        shared: Shared {
            config: AppConfig::default(),
            pipeline: Pipeline::new(None),
            store: crate::media::store::Store::default(),
            anim_store: crate::media::store::Store::default(),
            thumbs: crate::media::cache::ImageCache::new(crate::app::state::THUMB_BUDGET_BYTES),
            disk_cache_size: None,
            associations_registered: false,
            #[cfg(feature = "update-check")]
            update_status: None,
        },
        window: Window {
            id: window::Id::unique(),
            session: Session::Empty,
            open_menu: None,
            viewport_size: Size::new(800.0, 600.0),
            last_cursor_pos: iced::Point::ORIGIN,
            window_size: Size::new(800.0, 600.0),
            window_pos: iced::Point::ORIGIN,
            maximized: false,
            restored_size: Size::new(800.0, 600.0),
            restored_pos: iced::Point::ORIGIN,
            context_menu_pos: None,
            zoom_slider_open: false,
            fullscreen: false,
            focused: true,
            decay_generation: 0,
            minimized: false,
            video_resumes_on_restore: false,
            help_open: false,
            modal: None,
            opening_since: None,
            toasts: Vec::new(),
            next_toast_id: 0,
        },
    }
}

/// A headless app viewing the given file names, cursor on `cursor`.
pub(crate) fn viewing_app(names: &[&str], cursor: usize) -> TestApp {
    let files: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
    let start = files[cursor].clone();
    let nav = Nav::new(files, &start).unwrap();
    let viewer = Viewer::new(nav, Source::Fs, AnimPlayer::new());
    let mut app = empty_app();
    app.window.session = Session::Viewing(Box::new(viewer));
    app
}

/// Lift a single-window [`TestApp`] into a real [`App`], returning the id of
/// its one window. For tests that drive the top-level `update`/`view`.
pub(crate) fn into_app(app: TestApp) -> (App, window::Id) {
    let id = app.window.id;
    let mut windows = HashMap::new();
    windows.insert(id, app.window);
    (
        App {
            shared: app.shared,
            windows,
        },
        id,
    )
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
pub(crate) fn cache_thumb(app: &mut TestApp, path: &str, w: u32, h: u32) {
    let source = app
        .window
        .viewer()
        .map(|v| v.source.clone())
        .unwrap_or(Source::Fs);
    let thumb = thumb(w, h);
    let cost = thumb.byte_cost();
    app.shared
        .thumbs
        .insert(thumb_key(&source, std::path::Path::new(path)), thumb, cost);
}

/// Give `path` a resident leased image in the window's cache, backed by the
/// shared store (as if decoded and uploaded to a full-res texture), so the
/// viewer treats it as a resident sharp image with no real decode or GPU work.
pub(crate) fn cache_image(app: &mut TestApp, path: &str) {
    use crate::media::store::{ImageKey, RamImage, Tier};
    let source = app
        .window
        .viewer()
        .map(|v| v.source.clone())
        .unwrap_or(Source::Fs);
    let p = PathBuf::from(path);
    let key = ImageKey::new(&source, &p);
    let (lease, _) = app
        .shared
        .store
        .request(key.clone(), p.clone(), source, Tier::Full);
    app.shared.store.on_decoded(
        key.clone(),
        RamImage {
            handle: Handle::from_rgba(2, 2, vec![0u8; 16]),
            original_size: (2, 2),
            decode_time: None,
        },
    );
    app.shared
        .store
        .on_minted(key, Tier::Full, crate::ui::image_surface::test_keepalive());
    if let Some(viewer) = app.window.viewer_mut() {
        viewer.cache.insert(p, lease);
    }
}
