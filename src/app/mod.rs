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

/// Theme function: returns the active theme from config.
pub fn theme(app: &App) -> iced::Theme {
    match app.config.theme {
        crate::config::ThemeChoice::Dark => ui::theme::dark(),
        crate::config::ThemeChoice::Light => ui::theme::light(),
    }
}

/// Title function: returns the window title based on current state.
///
/// When the footer is hidden, the title bar includes the info that
/// would normally appear in the footer: file index, zoom, dimensions,
/// and file size.
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
