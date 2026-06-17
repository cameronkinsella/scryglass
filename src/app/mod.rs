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

mod message;
mod shortcuts;
pub mod state;
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
pub use message::Message;
pub use update::media::Message as MediaMessage;
pub use update::open::Message as OpenMessage;
pub use update::update;
pub use update::window::Message as WindowMessage;
pub use view::view;

use std::path::PathBuf;
use std::time::Duration;

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::{Event, Size, Subscription, Task, event, keyboard, mouse, window};

use crate::anim::AnimMessage;
use crate::components::info_panel;
use crate::components::toasts::Toast;
use crate::components::toolbar::OpenMenu;
use crate::config::AppConfig;
use crate::media::disk_thumbs::DiskThumbs;
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

/// Subscription: keyboard/mouse/file-drop events, GIF animation ticks,
/// and a redraw driver while the loading spinner is visible.
pub fn subscription(app: &App) -> Subscription<Message> {
    let mut subs = vec![
        event::listen_with(handle_event),
        // Close requests route through update() so config saves first.
        iced::window::close_requests().map(|id| Message::Window(WindowMessage::CloseRequested(id))),
    ];

    // The opening spinner runs before any viewer exists.
    if app.opening_since.is_some() {
        subs.push(
            iced::time::every(Duration::from_millis(33))
                .map(|_| Message::Media(MediaMessage::SpinnerTick)),
        );
    }

    if let Some(viewer) = app.viewer() {
        if viewer.pending_since.is_some() && app.opening_since.is_none() {
            subs.push(
                iced::time::every(Duration::from_millis(33))
                    .map(|_| Message::Media(MediaMessage::SpinnerTick)),
            );
        }

        if viewer.pending_since.is_none()
            && viewer.anim_player.is_animating()
            && let Some(delay) = viewer.anim_player.current_delay()
        {
            subs.push(iced::time::every(delay).map(|_| Message::Anim(AnimMessage::Tick)));
        }

        // Video pacing: pull frames due for display ~60×/s while a
        // session is active (paused sessions still need control redraws).
        if viewer.video.is_some() {
            subs.push(
                iced::time::every(Duration::from_millis(16))
                    .map(|_| Message::VideoControls(VideoControlsMessage::Tick)),
            );
        }
    }

    Subscription::batch(subs)
}

/// Returns true if the key is a forward navigation key (ArrowRight or D).
fn is_forward_key(key: &Key) -> bool {
    matches!(key, Key::Named(Named::ArrowRight))
        || matches!(key, Key::Character(c) if c.as_ref() == "d")
}

/// Returns true if the key is a backward navigation key (ArrowLeft or A).
fn is_backward_key(key: &Key) -> bool {
    matches!(key, Key::Named(Named::ArrowLeft))
        || matches!(key, Key::Character(c) if c.as_ref() == "a")
}

fn handle_event(event: Event, _status: event::Status, _id: window::Id) -> Option<Message> {
    match &event {
        // --- Keyboard: initial press ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_forward_key(key) => Some(Message::Viewer(ViewerMessage::Next)),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_backward_key(key) => Some(Message::Viewer(ViewerMessage::Prev)),

        // --- Keyboard: everything else goes through the shortcut table.
        // `modified_key` includes shift effects, so "?" and "R" arrive
        // as themselves rather than "/" and "r".
        Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key,
            modifiers,
            repeat: false,
            ..
        }) => shortcuts::map_press(modified_key, *modifiers),

        // --- Keyboard: OS key-repeat ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_forward_key(key) => Some(Message::Viewer(ViewerMessage::NextRepeat)),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_backward_key(key) => Some(Message::Viewer(ViewerMessage::PrevRepeat)),

        // --- Keyboard: key released ---
        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_forward_key(key) => {
            Some(Message::Viewer(ViewerMessage::NextReleased))
        }

        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_backward_key(key) => {
            Some(Message::Viewer(ViewerMessage::PrevReleased))
        }

        // --- Mouse: back/forward buttons (single navigation, no hold) ---
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Forward)) => {
            Some(Message::Viewer(ViewerMessage::Next))
        }
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Back)) => {
            Some(Message::Viewer(ViewerMessage::Prev))
        }

        // --- Mouse: cursor moved (for drag panning) ---
        Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::Viewer(ViewerMessage::DragMove(*position)))
        }
        Event::Mouse(mouse::Event::CursorLeft) => Some(Message::Viewer(ViewerMessage::CursorLeft)),

        // --- Mouse: left button released (end drag) ---
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::Viewer(ViewerMessage::DragEnd))
        }

        // --- File drop ---
        Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::Open(OpenMessage::FileDropped(path.clone())))
        }

        // --- Window resized ---
        Event::Window(window::Event::Resized(size)) => {
            Some(Message::Window(WindowMessage::Resized(*size)))
        }

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
