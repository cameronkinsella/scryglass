//! Application core (Elm Architecture).
//!
//! iced 0.14 uses free functions: boot() → State, update(&mut State, Message),
//! view(&State) → Element. The `application()` builder wires them together.
//!
//! Images are decoded by the [`crate::media`] pipeline and rendered through the
//! per-window image-surface shader, which owns the GPU texture (no cross-window
//! flicker, unlike iced's first-window-only atlas upload).
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
pub(crate) mod measure;
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
pub use message::{Envelope, Message};
pub use subscription::subscription;
pub use update::media::Message as MediaMessage;
pub use update::open::Message as OpenMessage;
pub use update::update;
pub use update::window::Message as WindowMessage;
pub use view::view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use iced::{Size, window};

use crate::components::info_panel;
use crate::components::toasts::Toast;
use crate::components::toolbar::OpenMenu;
use crate::config::AppConfig;
use crate::media::pipeline::Pipeline;
use crate::media::store::{Anim, Store};
use crate::ui;

use state::{Session, Viewer};

/// How long the arrow key must be held before continuous scrolling begins.
pub(crate) const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// Repeat interval for a held mouse edge press, which has no OS key-repeat to
/// pace it. The initial delay still comes from `HOLD_THRESHOLD`.
pub(crate) const EDGE_NAV_REPEAT: Duration = Duration::from_millis(90);

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
/// Per-tick opacity step for the control-bar fade (~5 ticks at the 16ms tick).
pub(crate) const CONTROLS_FADE_STEP: f32 = 0.2;

/// Application state: the single source of truth.
///
/// Split into [`Shared`] (one instance, common to every window) and a
/// [`Window`] per open viewer, keyed by its `window::Id`.
pub struct App {
    pub(crate) shared: Shared,
    pub(crate) windows: HashMap<window::Id, Window>,
}

/// Runtime bookkeeping for the Windows working-set trim. `generation` is bumped
/// on each background-state transition so a superseded timer no-ops; `armed`
/// records whether the trim condition already held, so only a transition arms a
/// new timer (the periodic minimize poll never resets a running one).
#[cfg(target_os = "windows")]
#[derive(Default)]
pub(crate) struct WorkingSet {
    pub(crate) generation: u64,
    pub(crate) armed: bool,
}

/// State common to every window: settings, the load pipeline, and global
/// facts shown in the per-window settings card.
pub struct Shared {
    /// Persisted settings (zoom mode, layout visibility, prefetch depth).
    pub(crate) config: AppConfig,
    /// Load orchestrator: cancellation generations and priority lanes.
    pub(crate) pipeline: Pipeline,
    /// The one owner of every still image's RAM and GPU texture, keyed by image
    /// identity and shared across all windows. Windows hold leases into it. It
    /// decides each image's tier from their aggregate demand.
    pub(crate) store: Store,
    /// The same store machinery for animations: the one owner of every GIF's decoded
    /// frames in RAM, keyed by image identity and shared across all windows. An
    /// animation has no GPU tier (each window composites its own frames), so this
    /// governs only the shared decoded RAM and its decay.
    pub(crate) anim_store: Store<Anim>,
    /// In-memory thumbnails (the filmstrip previews and the placeholder blur),
    /// shared across all windows and keyed by [`crate::media::pipeline::thumb_key`]
    /// so each image is thumbnailed once and every window reads the same copy.
    pub(crate) thumbs: crate::media::cache::ImageCache<crate::app::state::Thumb>,
    /// Probed size of the disk thumbnail store (settings display).
    pub(crate) disk_cache_size: Option<u64>,
    /// Whether the app is in the OS Open with menu (settings display,
    /// refreshed when the dialog opens).
    pub(crate) associations_registered: bool,
    /// Result of the last manual update check (settings). Ephemeral, cleared
    /// when settings closes so a reopen never shows a stale verdict.
    #[cfg(feature = "update-check")]
    pub(crate) update_status: Option<crate::update_check::UpdateStatus>,
    /// Process-global working-set trim bookkeeping (Windows only).
    #[cfg(target_os = "windows")]
    pub(crate) working_set: WorkingSet,
}

/// Everything tied to one viewer window.
pub struct Window {
    /// This window's own id, so window ops and per-window widget ids can use
    /// it directly instead of being threaded the id from the dispatcher.
    pub(crate) id: window::Id,
    pub(crate) session: Session,
    /// Which toolbar dropdown menu is open (if any).
    pub(crate) open_menu: Option<OpenMenu>,
    /// Size of the viewport (content area below toolbar, above footer).
    /// Updated on every window resize.
    pub(crate) viewport_size: Size,
    /// Extra chrome the config estimate misses (iced's spacing and padding around
    /// the toolbar, strips, and footer), learned from the measured image area so
    /// `recalc_viewport` tracks the true area during a resize without waiting on the
    /// async measurement. Zero until the first measurement calibrates it.
    pub(crate) chrome_pad: Size,
    /// Last known cursor position (updated on every CursorMoved event).
    pub(crate) last_cursor_pos: iced::Point,
    /// Bumped by every view change that debounces its tile demand pass, so
    /// only the LAST settle timer of a gesture fires. Lives on the window,
    /// not the viewer, so a stale timer can never match a fresh viewer.
    pub(crate) tile_epoch: u64,
    /// Last known window size (for recalculating viewport on layout toggles).
    pub(crate) window_size: Size,
    /// Last known window position (top-left, logical px), updated on move.
    pub(crate) window_pos: iced::Point,
    /// Whether the window is natively maximized, tracked so only the restored
    /// geometry is persisted.
    pub(crate) maximized: bool,
    /// The restored (plain windowed) size and position, tracked apart from the
    /// live size so a maximized or fullscreen window still remembers where it
    /// was. Persisted on close as the next window's geometry.
    pub(crate) restored_size: Size,
    pub(crate) restored_pos: iced::Point,
    /// Context menu position (window coords). `Some` when visible.
    pub(crate) context_menu_pos: Option<iced::Point>,
    /// Whether the footer zoom slider pop-up is open.
    pub(crate) zoom_slider_open: bool,
    /// Whether the window is borderless fullscreen (chrome hidden).
    pub(crate) fullscreen: bool,
    /// Whether this window currently has native focus. The resource states key
    /// VRAM retention on it.
    pub(crate) focused: bool,
    /// Bumped on every move or resize, so the settled state probe knows a
    /// mid-drag timer from the one armed by the final position.
    pub(crate) probe_generation: u64,
    /// Bumped on every focus or minimize change, so a deferred decay-stage
    /// timer can tell whether it is still current when it fires (a stale one
    /// no-ops). Replaces tracking a per-stage timestamp.
    pub(crate) decay_generation: u64,
    /// Whether the window is OS-minimized. A minimized window shows nothing,
    /// so it drives no redraw timers.
    pub(crate) minimized: bool,
    /// Whether an open video should resume when the window un-minimizes, set
    /// when it was playing at minimize time. A manual pause is not resumed.
    pub(crate) video_resumes_on_restore: bool,
    /// Whether the shortcut help overlay is open.
    pub(crate) help_open: bool,
    /// A blocking dialog over the viewer, if one is open. Keyboard-driven
    /// viewer interactions are inert while this is `Some`.
    pub(crate) modal: Option<Modal>,
    /// When an open started (directory scan or archive indexing), until
    /// its listing arrives. Drives the spinner for slow archives.
    pub(crate) opening_since: Option<iced::time::Instant>,
    /// Live toast notifications, oldest first.
    pub(crate) toasts: Vec<Toast>,
    /// Monotonic toast ID source.
    pub(crate) next_toast_id: u64,
}

impl Window {
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
    /// Rename the file in place. `format` is the file's sniffed format, used to
    /// flag a rename that would mislabel it (`None` if it couldn't be sniffed).
    Rename {
        input: String,
        format: Option<crate::media::FileFormat>,
    },
    /// The settings card.
    Settings,
}

/// The active theme, from config. Shared across all windows.
pub fn theme(app: &App, _id: window::Id) -> iced::Theme {
    match app.shared.config.theme {
        crate::config::ThemeChoice::Dark => ui::theme::dark(),
        crate::config::ThemeChoice::Light => ui::theme::light(),
    }
}

/// The window title. With the footer hidden, it also carries the
/// footer's info: file index, zoom, dimensions, and size.
pub fn title(app: &App, id: window::Id) -> String {
    let Some(viewer) = app.windows.get(&id).and_then(Window::viewer) else {
        return String::new();
    };

    let filename = viewer
        .nav
        .current()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if app.shared.config.show_footer {
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
pub(crate) fn recalc_viewport(win: &mut Window, shared: &Shared) {
    if win.fullscreen {
        // Fullscreen hides all chrome: the image owns the whole window.
        win.viewport_size = win.window_size;
        return;
    }
    let chrome_width = if shared.config.show_info {
        info_panel::WIDTH
    } else {
        0.0
    };
    let mut chrome_height: f32 = if shared.config.show_toolbar {
        TOOLBAR_HEIGHT
    } else {
        0.0
    };
    if shared.config.show_filmstrip {
        chrome_height += 72.0; // filmstrip + padding
    }
    if shared.config.show_slider {
        chrome_height += 28.0; // slider + padding
    }
    if shared.config.show_footer {
        chrome_height += 25.0; // footer
    }
    // Subtract the learned pad so the estimate matches iced's real layout, which the
    // config heights alone miss by a few pixels of spacing.
    win.viewport_size = Size::new(
        (win.window_size.width - chrome_width - win.chrome_pad.width).max(1.0),
        (win.window_size.height - chrome_height - win.chrome_pad.height).max(1.0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, into_app, viewing_app};

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
        app.window.window_size = Size::new(1280.0, 720.0);
        app.window.fullscreen = true;
        recalc_viewport(&mut app.window, &app.shared);
        assert_eq!(app.window.viewport_size, app.window.window_size);
    }

    #[test]
    fn viewport_equals_window_when_all_chrome_is_hidden() {
        let mut app = empty_app();
        app.window.window_size = Size::new(1280.0, 720.0);
        app.shared.config.show_toolbar = false;
        app.shared.config.show_info = false;
        app.shared.config.show_filmstrip = false;
        app.shared.config.show_slider = false;
        app.shared.config.show_footer = false;
        recalc_viewport(&mut app.window, &app.shared);
        assert_eq!(app.window.viewport_size, Size::new(1280.0, 720.0));
    }

    #[test]
    fn toolbar_takes_its_height_from_the_viewport() {
        let mut app = empty_app();
        app.window.window_size = Size::new(1280.0, 720.0);
        app.shared.config.show_toolbar = true;
        app.shared.config.show_info = false;
        app.shared.config.show_filmstrip = false;
        app.shared.config.show_slider = false;
        app.shared.config.show_footer = false;
        recalc_viewport(&mut app.window, &app.shared);
        assert_eq!(app.window.viewport_size.width, 1280.0);
        assert_eq!(app.window.viewport_size.height, 720.0 - TOOLBAR_HEIGHT);
    }

    #[test]
    fn title_is_the_filename_when_the_footer_is_shown() {
        let mut t = viewing_app(&["photo.png", "b.png"], 0);
        t.shared.config.show_footer = true;
        let (app, id) = into_app(t);
        assert_eq!(title(&app, id), "photo.png");
    }

    #[test]
    fn title_without_footer_shows_placeholders_for_unknown_dimensions() {
        let mut t = viewing_app(&["photo.png"], 0);
        t.shared.config.show_footer = false;
        let (app, id) = into_app(t);
        let title = title(&app, id);
        assert!(title.contains("photo.png"));
        assert!(title.contains('…'));
    }

    #[test]
    fn title_is_empty_without_a_viewer() {
        let (app, id) = into_app(empty_app());
        assert_eq!(title(&app, id), "");
    }
}
