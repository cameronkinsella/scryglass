//! Application core (Elm Architecture).
//!
//! iced 0.14 uses free functions: boot() → State, update(&mut State, Message),
//! view(&State) → Element. The `application()` builder wires them together.
//!
//! Images are decoded by the [`crate::media`] pipeline and uploaded as GPU
//! `Allocation`s, which are guaranteed to render immediately (no flicker).
//!
//! Navigation never blocks: every keypress moves the cursor. Cache hits
//! display instantly, misses keep the previous image visible while a
//! cancellable load runs. Stale loads (the user has moved on) are cancelled
//! via a generation counter.
//!
//! A short press moves exactly one image. Continuous scrolling only begins
//! after the key has been held for a brief threshold (`HOLD_THRESHOLD`),
//! driven by OS key-repeat events.

mod boot;
mod message;
mod shortcuts;
pub mod state;
mod subscription;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod update;
mod view;
pub(crate) mod viewer_math;

pub use crate::components::context_menu::Message as ContextMenuMessage;
pub use crate::components::filmstrip::Message as FilmstripMessage;
pub use crate::components::modal::Message as ModalMessage;
pub use crate::components::nav_slider::Message as SliderMessage;
pub use crate::components::settings::Message as SettingsMessage;
pub use crate::components::toolbar::Message as ToolbarMessage;
pub use crate::components::video_controls::Message as VideoControlsMessage;
pub use crate::components::video_controls::Message as VideoMessage;
pub use crate::components::viewer::Message as ViewerMessage;
pub use boot::boot;
pub use message::Message;
pub use subscription::subscription;
pub use update::media::Message as MediaMessage;
pub use update::open::Message as OpenMessage;
pub use update::update;
pub use update::window::Message as WindowMessage;
pub use view::view;

use std::path::PathBuf;
use std::time::Duration;

use iced::Size;

use crate::components::info_panel;
use crate::components::toasts::Toast;
use crate::components::toolbar::OpenMenu;
use crate::config::AppConfig;
use crate::media::pipeline::Pipeline;
use crate::ui;

use state::{Session, Viewer};

/// How long the arrow key must be held before continuous scrolling begins.
pub(crate) const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// Scroll-wheel zoom step multiplier (each notch = ×1.1 or ÷1.1).
pub(crate) const ZOOM_STEP: f32 = 1.1;

/// Minimum zoom factor.
pub(crate) const ZOOM_MIN: f32 = 0.01;

/// Maximum zoom factor.
pub(crate) const ZOOM_MAX: f32 = 50.0;

/// Height of the toolbar in logical pixels.
pub(crate) const TOOLBAR_HEIGHT: f32 = 30.0;

/// Grace period before the loading spinner appears, so fast loads finish
/// without any flash of UI.
pub(crate) const SPINNER_DELAY: Duration = Duration::from_millis(150);

/// How long the video controls stay up after the last mouse movement.
pub(crate) const VIDEO_CONTROLS_TIMEOUT: Duration = Duration::from_millis(2500);

/// Application state: the single source of truth.
pub struct App {
    pub(crate) session: Session,
    /// Persisted settings (zoom mode, layout visibility, prefetch depth).
    pub(crate) config: AppConfig,
    /// Load orchestrator: cancellation generations and priority lanes.
    pub(crate) pipeline: Pipeline,
    /// Which toolbar dropdown menu is open (if any).
    pub(crate) open_menu: Option<OpenMenu>,
    /// Size of the viewport (content area below toolbar, above footer).
    /// Updated on every window resize.
    pub(crate) viewport_size: Size,
    /// Last known cursor position (updated on every CursorMoved event).
    pub(crate) last_cursor_pos: iced::Point,
    /// Last known window size (for recalculating viewport on layout toggles).
    pub(crate) window_size: Size,
    /// Context menu position (window coords). `Some` when visible.
    pub(crate) context_menu_pos: Option<iced::Point>,
    /// Whether the footer zoom slider pop-up is open.
    pub(crate) zoom_slider_open: bool,
    /// Whether the window is borderless fullscreen (chrome hidden).
    pub(crate) fullscreen: bool,
    /// Whether the shortcut help overlay is open.
    pub(crate) help_open: bool,
    /// A blocking dialog over the viewer, if one is open. Keyboard-driven
    /// viewer interactions are inert while this is `Some`.
    pub(crate) modal: Option<Modal>,
    /// Probed size of the disk thumbnail store (settings display).
    pub(crate) disk_cache_size: Option<u64>,
    /// Whether the app is in the OS Open with menu (settings display,
    /// refreshed when the dialog opens).
    pub(crate) associations_registered: bool,
    /// When an open started (directory scan or archive indexing), until
    /// its listing arrives. Drives the spinner for slow archives.
    pub(crate) opening_since: Option<iced::time::Instant>,
    /// Live toast notifications, oldest first.
    pub(crate) toasts: Vec<Toast>,
    /// Monotonic toast ID source.
    pub(crate) next_toast_id: u64,
}

impl App {
    /// The active viewer, if any.
    pub(crate) fn viewer(&self) -> Option<&Viewer> {
        match &self.session {
            Session::Viewing(viewer) => Some(viewer),
            Session::Empty => None,
        }
    }

    /// The active viewer, mutably, if any.
    pub(crate) fn viewer_mut(&mut self) -> Option<&mut Viewer> {
        match &mut self.session {
            Session::Viewing(viewer) => Some(viewer),
            Session::Empty => None,
        }
    }
}

/// A blocking dialog over the viewer.
pub enum Modal {
    /// Confirm moving the file to the recycle bin.
    ConfirmDelete(PathBuf),
    /// Rename the file in place.
    Rename { input: String },
    /// The settings card.
    Settings,
}

/// The active theme, from config.
pub fn theme(app: &App) -> iced::Theme {
    match app.config.theme {
        crate::config::ThemeChoice::Dark => ui::theme::dark(),
        crate::config::ThemeChoice::Light => ui::theme::light(),
    }
}

/// The window title. With the footer hidden, it also carries the
/// footer's info: file index, zoom, dimensions, and size.
pub fn title(app: &App) -> String {
    let Some(viewer) = app.viewer() else {
        return String::new();
    };

    let filename = viewer
        .nav
        .current()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if app.config.show_footer {
        return filename;
    }

    let position = viewer.nav.position_label();

    let (dims, zoom) = match viewer.displayed.original_size() {
        Some((w, h)) => (
            ui::format_dimensions(w, h),
            format!("{}%", (viewer.zoom * 100.0).round() as u32),
        ),
        None => ("…".to_string(), "…".to_string()),
    };

    let size = ui::file_size_label(viewer.current_file_size);

    format!("{filename}  |  {position}  |  {zoom}  |  {dims}  |  {size}")
}

/// Recalculate the viewport size based on window size and visible chrome.
pub(crate) fn recalc_viewport(app: &mut App) {
    if app.fullscreen {
        // Fullscreen hides all chrome: the image owns the whole window.
        app.viewport_size = app.window_size;
        return;
    }
    let chrome_width = if app.config.show_info {
        info_panel::WIDTH
    } else {
        0.0
    };
    let mut chrome_height: f32 = if app.config.show_toolbar {
        TOOLBAR_HEIGHT
    } else {
        0.0
    };
    if app.config.show_filmstrip {
        chrome_height += 72.0; // filmstrip + padding
    }
    if app.config.show_slider {
        chrome_height += 28.0; // slider + padding
    }
    if app.config.show_footer {
        chrome_height += 25.0; // footer
    }
    app.viewport_size = Size::new(
        (app.window_size.width - chrome_width).max(1.0),
        (app.window_size.height - chrome_height).max(1.0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, viewing_app};

    #[test]
    fn viewer_accessors_track_the_session() {
        assert!(empty_app().viewer().is_none());
        let mut viewing = viewing_app(&["a.png"], 0);
        assert!(viewing.viewer().is_some());
        assert!(viewing.viewer_mut().is_some());
    }

    #[test]
    fn fullscreen_viewport_fills_the_window() {
        let mut app = empty_app();
        app.window_size = Size::new(1280.0, 720.0);
        app.fullscreen = true;
        recalc_viewport(&mut app);
        assert_eq!(app.viewport_size, app.window_size);
    }

    #[test]
    fn viewport_equals_window_when_all_chrome_is_hidden() {
        let mut app = empty_app();
        app.window_size = Size::new(1280.0, 720.0);
        app.config.show_toolbar = false;
        app.config.show_info = false;
        app.config.show_filmstrip = false;
        app.config.show_slider = false;
        app.config.show_footer = false;
        recalc_viewport(&mut app);
        assert_eq!(app.viewport_size, Size::new(1280.0, 720.0));
    }

    #[test]
    fn toolbar_takes_its_height_from_the_viewport() {
        let mut app = empty_app();
        app.window_size = Size::new(1280.0, 720.0);
        app.config.show_toolbar = true;
        app.config.show_info = false;
        app.config.show_filmstrip = false;
        app.config.show_slider = false;
        app.config.show_footer = false;
        recalc_viewport(&mut app);
        assert_eq!(app.viewport_size.width, 1280.0);
        assert_eq!(app.viewport_size.height, 720.0 - TOOLBAR_HEIGHT);
    }

    #[test]
    fn title_is_the_filename_when_the_footer_is_shown() {
        let mut app = viewing_app(&["photo.png", "b.png"], 0);
        app.config.show_footer = true;
        assert_eq!(title(&app), "photo.png");
    }

    #[test]
    fn title_without_footer_shows_placeholders_for_unknown_dimensions() {
        let mut app = viewing_app(&["photo.png"], 0);
        app.config.show_footer = false;
        let title = title(&app);
        assert!(title.contains("photo.png"));
        assert!(title.contains('…'));
    }

    #[test]
    fn title_is_empty_without_a_viewer() {
        assert_eq!(title(&empty_app()), "");
    }
}
