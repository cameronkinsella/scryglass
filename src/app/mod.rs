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
pub mod state;
mod update;
mod view;
mod viewer_math;

pub use message::Message;
pub use update::update;
pub use view::view;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::{Event, Size, Subscription, Task, event, keyboard, mouse, window};

use crate::anim::AnimMessage;
use crate::config::AppConfig;
use crate::media::disk_thumbs::DiskThumbs;
use crate::media::pipeline::Pipeline;
use crate::ui;
use crate::ui::toast::Toast;
use crate::ui::toolbar::OpenMenu;

use state::{Session, Viewer};

/// How long the arrow key must be held before continuous scrolling begins.
const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// Scroll-wheel zoom step multiplier (each notch = ×1.1 or ÷1.1).
const ZOOM_STEP: f32 = 1.1;

/// Minimum zoom factor.
const ZOOM_MIN: f32 = 0.01;

/// Maximum zoom factor.
const ZOOM_MAX: f32 = 50.0;

/// Height of the toolbar in logical pixels.
const TOOLBAR_HEIGHT: f32 = 30.0;

/// Grace period before the loading spinner appears, so fast loads finish
/// without any flash of UI.
const SPINNER_DELAY: Duration = Duration::from_millis(150);

/// Application state: the single source of truth.
pub struct App {
    session: Session,
    /// Persisted settings (zoom mode, layout visibility, prefetch depth).
    config: AppConfig,
    /// Load orchestrator: cancellation generations and priority lanes.
    pipeline: Pipeline,
    /// Which toolbar dropdown menu is open (if any).
    open_menu: Option<OpenMenu>,
    /// Size of the viewport (content area below toolbar, above footer).
    /// Updated on every window resize.
    viewport_size: Size,
    /// Last known cursor position (updated on every CursorMoved event).
    last_cursor_pos: iced::Point,
    /// Last known window size (for recalculating viewport on layout toggles).
    window_size: Size,
    /// Context menu position (window coords). `Some` when visible.
    context_menu_pos: Option<iced::Point>,
    /// Whether the window is borderless fullscreen (chrome hidden).
    fullscreen: bool,
    /// Whether the shortcut help overlay is open.
    help_open: bool,
    /// A blocking dialog over the viewer, if one is open. Keyboard-driven
    /// viewer interactions are inert while this is `Some`.
    modal: Option<Modal>,
    /// Probed size of the disk thumbnail store (settings display).
    disk_cache_size: Option<u64>,
    /// Live toast notifications, oldest first.
    toasts: Vec<Toast>,
    /// Monotonic toast ID source.
    next_toast_id: u64,
}

impl App {
    /// The active viewer, if any.
    fn viewer(&self) -> Option<&Viewer> {
        match &self.session {
            Session::Viewing(viewer) => Some(viewer),
            Session::Empty => None,
        }
    }

    /// The active viewer, mutably, if any.
    fn viewer_mut(&mut self) -> Option<&mut Viewer> {
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
pub fn boot() -> (App, Task<Message>) {
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
        toasts: Vec::new(),
        next_toast_id: 0,
    };
    recalc_viewport(&mut app);

    let open = initial_open_arg(std::env::args_os())
        .map(update::open_path)
        .unwrap_or_else(Task::none);

    (app, Task::batch([housekeeping, open]))
}

/// The first CLI argument as a path, if it points to an existing file
/// or directory.
fn initial_open_arg(mut args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    let _exe = args.next();
    let path = PathBuf::from(args.next()?);
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
fn recalc_viewport(app: &mut App) {
    if app.fullscreen {
        // Fullscreen hides all chrome: the image owns the whole window.
        app.viewport_size = app.window_size;
        return;
    }
    let chrome_width = if app.config.show_info {
        ui::info_panel::WIDTH
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
        iced::window::close_requests().map(Message::CloseRequested),
    ];

    if let Some(viewer) = app.viewer() {
        if viewer.pending_since.is_some() {
            subs.push(iced::time::every(Duration::from_millis(33)).map(|_| Message::SpinnerTick));
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
            subs.push(iced::time::every(Duration::from_millis(16)).map(|_| Message::VideoTick));
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
        }) if is_forward_key(key) => Some(Message::Next),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_backward_key(key) => Some(Message::Prev),

        // --- Keyboard: everything else goes through the shortcut table.
        // `modified_key` includes shift effects, so "?" and "R" arrive
        // as themselves rather than "/" and "r".
        Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key,
            modifiers,
            repeat: false,
            ..
        }) => ui::shortcuts::map_press(modified_key, *modifiers),

        // --- Keyboard: OS key-repeat ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_forward_key(key) => Some(Message::NextRepeat),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_backward_key(key) => Some(Message::PrevRepeat),

        // --- Keyboard: key released ---
        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_forward_key(key) => {
            Some(Message::NextReleased)
        }

        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_backward_key(key) => {
            Some(Message::PrevReleased)
        }

        // --- Mouse: back/forward buttons (single navigation, no hold) ---
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Forward)) => Some(Message::Next),
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Back)) => Some(Message::Prev),

        // --- Mouse: cursor moved (for drag panning) ---
        Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::DragMove(*position)),

        // --- Mouse: left button released (end drag) ---
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => Some(Message::DragEnd),

        // --- File drop ---
        Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path.clone())),

        // --- Window resized ---
        Event::Window(window::Event::Resized(size)) => Some(Message::WindowResized(*size)),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn args(items: &[&std::path::Path]) -> impl Iterator<Item = OsString> {
        std::iter::once(OsString::from("scryglass.exe"))
            .chain(items.iter().map(|p| p.as_os_str().to_owned()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn initial_open_arg_returns_existing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("photo.png");
        fs::write(&file, b"").unwrap();
        assert_eq!(initial_open_arg(args(&[&file])), Some(file));
    }

    #[test]
    fn initial_open_arg_returns_existing_directory() {
        let dir = TempDir::new().unwrap();
        assert_eq!(
            initial_open_arg(args(&[dir.path()])),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn initial_open_arg_rejects_missing_path() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope.png");
        assert_eq!(initial_open_arg(args(&[&missing])), None);
    }

    #[test]
    fn initial_open_arg_without_args_returns_none() {
        assert_eq!(initial_open_arg(args(&[])), None);
    }

    #[test]
    fn initial_open_arg_ignores_extra_args() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.png");
        fs::write(&file, b"").unwrap();
        let extra = dir.path().join("b.png");
        fs::write(&extra, b"").unwrap();
        assert_eq!(initial_open_arg(args(&[&file, &extra])), Some(file));
    }
}
