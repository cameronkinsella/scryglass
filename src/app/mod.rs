//! Application core (Elm Architecture).
//!
//! iced 0.14 uses free functions: boot() → State, update(&mut State, Message),
//! view(&State) → Element. The `application()` builder wires them together.
//!
//! Images are loaded via `image::allocate()`, which returns an `Allocation`,
//! a GPU-resident texture guaranteed to render immediately (no flicker).
//!
//! Navigation is gated on image load: the cursor does not advance until the
//! current image's `Allocation` arrives. During a key-hold, the next navigation
//! fires only once the previous image is ready, no queued-up navigations.
//!
//! A short press moves exactly one image. Continuous scrolling only begins
//! after the key has been held for a brief threshold (`HOLD_THRESHOLD`),
//! driven by OS key-repeat events.

mod message;
mod state;
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

use crate::config::AppConfig;
use crate::gif::GifMessage;
use crate::ui;
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

/// Application state: the single source of truth.
pub struct App {
    session: Session,
    /// Persisted settings (zoom mode, layout visibility, prefetch depth).
    config: AppConfig,
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

/// Boot function: creates the initial state. Called once by iced.
///
/// If a file or directory path was passed on the command line (e.g. via
/// "Open with…" in a file manager), opening it starts immediately.
pub fn boot() -> (App, Task<Message>) {
    let mut app = App {
        session: Session::Empty,
        config: AppConfig::load(),
        open_menu: None,
        viewport_size: Size::new(800.0, 600.0),
        last_cursor_pos: iced::Point::ORIGIN,
        window_size: Size::new(800.0, 600.0),
        context_menu_pos: None,
    };
    recalc_viewport(&mut app);

    let task = initial_open_arg(std::env::args_os())
        .map(update::open_path)
        .unwrap_or_else(Task::none);

    (app, task)
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
    let zoom_pct = (viewer.zoom * 100.0).round() as u32;

    let dims = viewer
        .current_allocation
        .as_ref()
        .map(|a| {
            let s = a.size();
            ui::format_dimensions(s.width, s.height)
        })
        .unwrap_or_default();

    let size = ui::format_file_size(viewer.current_file_size);

    format!("{filename}  |  {position}  |  {zoom_pct}%  |  {dims}  |  {size}")
}

/// Recalculate the viewport size based on window size and visible chrome.
fn recalc_viewport(app: &mut App) {
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
        app.window_size.width,
        (app.window_size.height - chrome_height).max(1.0),
    );
}

/// Subscription: listens for keyboard/mouse/file-drop events, plus GIF animation ticks.
pub fn subscription(app: &App) -> Subscription<Message> {
    let events = event::listen_with(handle_event);

    if let Some(viewer) = app.viewer()
        && !viewer.loading
        && viewer.gif_player.is_animating()
        && let Some(delay) = viewer.gif_player.current_delay()
    {
        let tick = iced::time::every(delay).map(|_| Message::Gif(GifMessage::Tick));
        return Subscription::batch([events, tick]);
    }

    events
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
        // --- Keyboard: Escape dismisses open menus ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        }) => Some(Message::DismissOverlay),

        // --- Keyboard: initial press ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_forward_key(key) => Some(Message::Next),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_backward_key(key) => Some(Message::Prev),

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
