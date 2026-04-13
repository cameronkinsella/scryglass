//! Application state, messages, and update/view logic (Elm Architecture).
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

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::time::Instant;
use iced::widget::image::Allocation;
use iced::widget::{Stack, column, mouse_area};
use iced::window;
use iced::{Element, Event, Length, Subscription, Task, event, keyboard, mouse};

use crate::cache;
use crate::config::AppConfig;
use crate::gif::{self, GifMessage, GifPlayer};
use crate::nav::{self, Nav};
use crate::widgets;

/// How long the arrow key must be held before continuous scrolling begins.
const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// Application state: the single source of truth.
pub struct App {
    state: AppState,
    config: AppConfig,
    /// Whether the File dropdown menu is open.
    file_menu_open: bool,
}

enum AppState {
    /// Waiting for a file drop.
    Empty,
    /// Actively viewing images.
    Viewing {
        nav: Nav,
        /// GPU-allocated texture for the current image / current GIF frame.
        /// Once set, this is NEVER set to `None`.
        current_allocation: Option<Allocation>,
        /// Pre-allocated textures for neighbor images.
        _prefetch_allocations: Vec<Allocation>,
        /// True while waiting for the current image's allocation.
        loading: bool,
        /// Which direction key is currently held, and when the hold started.
        held_direction: Option<(Direction, Instant)>,
        /// Animated GIF player that handles decode cache and animation.
        gif_player: GifPlayer,
        /// Cached file size in bytes of the current image (set on load).
        current_file_size: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Clone)]
pub enum Message {
    FileDropped(PathBuf),
    DirectoryScanned(PathBuf, Result<Vec<PathBuf>, String>),
    /// A static image allocation completed (current or prefetch).
    ImageAllocated(PathBuf, Result<Allocation, cache::Error>),
    /// Wrapped GIF player message.
    Gif(GifMessage),
    /// Navigate forward (initial press).
    Next,
    /// Navigate backward (initial press).
    Prev,
    /// Navigate forward (OS key-repeat).
    NextRepeat,
    /// Navigate backward (OS key-repeat).
    PrevRepeat,
    /// Forward key released.
    NextReleased,
    /// Backward key released.
    PrevReleased,
    /// Toggle the File dropdown menu.
    ToggleFileMenu,
    /// Dismiss any open overlay (click outside menu).
    DismissOverlay,
    /// Open a file via native dialog.
    OpenFile,
    /// File dialog completed.
    FileDialogResult(Option<PathBuf>),
    /// Close the current image (return to empty state).
    CloseFile,
    /// Quit the application.
    Quit,
}

/// Boot function: creates the initial state. Called once by iced.
pub fn boot() -> App {
    App {
        state: AppState::Empty,
        config: AppConfig::default(),
        file_menu_open: false,
    }
}

/// Title function: returns the window title based on current state.
pub fn title(app: &App) -> String {
    match &app.state {
        AppState::Empty => String::from("scryglass"),
        AppState::Viewing { nav, .. } => {
            let name = nav
                .current()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{name} - scryglass")
        }
    }
}

/// Update function: handles messages and mutates state.
pub fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::FileDropped(path) => open_path(path),

        Message::DirectoryScanned(start_file, Ok(files)) => match Nav::new(files, &start_file) {
            Ok(nav) => {
                let gif_player = GifPlayer::new();
                let tasks = load_current_and_prefetch(&nav, &gif_player, app.config.prefetch_depth);
                let file_size = std::fs::metadata(nav.current())
                    .map(|m| m.len())
                    .unwrap_or(0);
                app.state = AppState::Viewing {
                    nav,
                    current_allocation: None,
                    _prefetch_allocations: Vec::new(),
                    loading: true,
                    held_direction: None,
                    gif_player,
                    current_file_size: file_size,
                };
                tasks
            }
            Err(_) => Task::none(),
        },

        Message::DirectoryScanned(_start_file, Err(_err)) => Task::none(),

        Message::ImageAllocated(path, Ok(allocation)) => {
            let AppState::Viewing {
                nav,
                current_allocation,
                _prefetch_allocations,
                loading,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            if nav.current() == path {
                *current_allocation = Some(allocation);
                *loading = false;
            } else {
                _prefetch_allocations.push(allocation);
            }
            Task::none()
        }

        Message::ImageAllocated(_path, Err(_err)) => Task::none(),

        Message::Gif(gif_msg) => {
            let AppState::Viewing {
                nav,
                current_allocation,
                loading,
                gif_player,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            let (task, allocation) = gif_player.update(gif_msg, nav.current());

            if let Some(alloc) = allocation {
                *current_allocation = Some(alloc);
                *loading = false;
            }

            task.map(Message::Gif)
        }

        // --- Initial press: always navigate + record hold start ---
        Message::Next => {
            let AppState::Viewing {
                loading,
                held_direction,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };
            *held_direction = Some((Direction::Forward, Instant::now()));
            if *loading {
                return Task::none();
            }
            navigate(app, Direction::Forward)
        }

        Message::Prev => {
            let AppState::Viewing {
                loading,
                held_direction,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };
            *held_direction = Some((Direction::Backward, Instant::now()));
            if *loading {
                return Task::none();
            }
            navigate(app, Direction::Backward)
        }

        // --- OS key-repeat: only navigate if held past threshold ---
        Message::NextRepeat => {
            let AppState::Viewing {
                loading,
                held_direction,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };
            let past = held_direction
                .map(|(_, t)| t.elapsed() >= HOLD_THRESHOLD)
                .unwrap_or(false);
            if !past || *loading {
                return Task::none();
            }
            navigate(app, Direction::Forward)
        }

        Message::PrevRepeat => {
            let AppState::Viewing {
                loading,
                held_direction,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };
            let past = held_direction
                .map(|(_, t)| t.elapsed() >= HOLD_THRESHOLD)
                .unwrap_or(false);
            if !past || *loading {
                return Task::none();
            }
            navigate(app, Direction::Backward)
        }

        // --- Key released: stop continuous scrolling ---
        Message::NextReleased => {
            if let AppState::Viewing { held_direction, .. } = &mut app.state
                && held_direction
                    .map(|(d, _)| d == Direction::Forward)
                    .unwrap_or(false)
            {
                *held_direction = None;
            }
            Task::none()
        }

        Message::PrevReleased => {
            if let AppState::Viewing { held_direction, .. } = &mut app.state
                && held_direction
                    .map(|(d, _)| d == Direction::Backward)
                    .unwrap_or(false)
            {
                *held_direction = None;
            }
            Task::none()
        }

        // --- Menu state ---
        Message::ToggleFileMenu => {
            app.file_menu_open = !app.file_menu_open;
            Task::none()
        }

        Message::DismissOverlay => {
            app.file_menu_open = false;
            Task::none()
        }

        // --- File menu actions ---
        Message::OpenFile => {
            app.file_menu_open = false;
            let extensions = AppConfig::supported_extensions()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            Task::perform(
                async move {
                    let handle = rfd::AsyncFileDialog::new()
                        .add_filter(
                            "Images",
                            &extensions.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        )
                        .pick_file()
                        .await;
                    handle.map(|h| h.path().to_path_buf())
                },
                Message::FileDialogResult,
            )
        }

        Message::FileDialogResult(Some(path)) => open_path(path),
        Message::FileDialogResult(None) => Task::none(),

        Message::CloseFile => {
            app.file_menu_open = false;
            app.state = AppState::Empty;
            Task::none()
        }

        Message::Quit => iced::exit(),
    }
}

/// Shared logic for opening a file (from drop or dialog).
fn open_path(path: PathBuf) -> Task<Message> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let dropped = path;
    Task::perform(
        async move {
            let result = nav::scan_directory(&dir);
            match result {
                Ok(files) => (dropped, Ok(files)),
                Err(e) => (dropped, Err(e.to_string())),
            }
        },
        |(path, result)| Message::DirectoryScanned(path, result),
    )
}

/// Navigate to the next/prev image.
fn navigate(app: &mut App, direction: Direction) -> Task<Message> {
    let AppState::Viewing {
        nav,
        _prefetch_allocations,
        loading,
        gif_player,
        current_file_size,
        ..
    } = &mut app.state
    else {
        return Task::none();
    };

    match direction {
        Direction::Forward => nav.next(),
        Direction::Backward => nav.prev(),
    }

    *loading = true;
    gif_player.stop();
    _prefetch_allocations.clear();

    *current_file_size = std::fs::metadata(nav.current())
        .map(|m| m.len())
        .unwrap_or(0);

    let keep: HashSet<PathBuf> = {
        let mut set = HashSet::new();
        set.insert(nav.current().to_path_buf());
        for p in nav.peek_around(app.config.prefetch_depth) {
            set.insert(p);
        }
        set
    };
    gif_player.prune_cache(&keep);

    let current_path = nav.current().to_path_buf();
    if gif::is_gif(&current_path)
        && let Some(gif_task) = gif_player.try_start_from_cache(&current_path)
    {
        let prefetch = prefetch_neighbors(nav, gif_player, app.config.prefetch_depth);
        return Task::batch([gif_task.map(Message::Gif), prefetch]);
    }

    load_current_and_prefetch(nav, gif_player, app.config.prefetch_depth)
}

/// Fire allocation/decode tasks for the current image and its neighbors.
fn load_current_and_prefetch(nav: &Nav, gif_player: &GifPlayer, depth: usize) -> Task<Message> {
    let current_path = nav.current().to_path_buf();

    let current_task = if gif::is_gif(&current_path) {
        gif_player.decode_current(&current_path).map(Message::Gif)
    } else {
        let p = current_path.clone();
        cache::allocate_path(&p).map(move |result| Message::ImageAllocated(p.clone(), result))
    };

    let prefetch = prefetch_neighbors(nav, gif_player, depth);
    Task::batch([current_task, prefetch])
}

/// Fire prefetch tasks for neighbor images/GIFs.
fn prefetch_neighbors(nav: &Nav, gif_player: &GifPlayer, depth: usize) -> Task<Message> {
    let tasks: Vec<Task<Message>> = nav
        .peek_around(depth)
        .into_iter()
        .map(|p| {
            if gif::is_gif(&p) {
                gif_player.prefetch_decode(&p).map(Message::Gif)
            } else {
                let p2 = p.clone();
                cache::allocate_path(&p)
                    .map(move |result| Message::ImageAllocated(p2.clone(), result))
            }
        })
        .collect();
    Task::batch(tasks)
}

/// View function: assembles toolbar, content area, and footer.
pub fn view(app: &App) -> Element<'_, Message> {
    let toolbar = widgets::toolbar::menu_bar(app.file_menu_open);

    let content = match &app.state {
        AppState::Empty => widgets::image_display::drop_prompt(),
        AppState::Viewing {
            nav,
            current_allocation,
            current_file_size,
            ..
        } => match current_allocation {
            Some(allocation) => {
                let size = allocation.size();
                let footer = widgets::footer::footer(
                    &widgets::format_dimensions(size.width, size.height),
                    &widgets::format_file_size(*current_file_size),
                    &nav.position_label(),
                );
                column![widgets::image_display::image_display(allocation), footer,].into()
            }
            None => widgets::image_display::loading_prompt(),
        },
    };

    // Main layout: toolbar on top, then content fills remaining space.
    // If the file menu is open, layer the dropdown on top of the content
    // area so it floats over the image instead of pushing it down.
    if let Some(dropdown) = widgets::toolbar::dropdown(app.file_menu_open) {
        let overlay = column![dropdown].width(Length::Fill).height(Length::Fill);

        let stacked = Stack::with_children(vec![content, overlay.into()]);
        // mouse_area wraps the whole view so clicking outside dismisses the menu.
        let page = column![toolbar, stacked];
        mouse_area(page).on_press(Message::DismissOverlay).into()
    } else {
        column![toolbar, content].into()
    }
}

/// Subscription: listens for keyboard/mouse/file-drop events, plus GIF animation ticks.
pub fn subscription(app: &App) -> Subscription<Message> {
    let events = event::listen_with(handle_event);

    if let AppState::Viewing {
        gif_player,
        loading: false,
        ..
    } = &app.state
        && gif_player.is_animating()
        && let Some(delay) = gif_player.current_delay()
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

        // --- File drop ---
        Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path.clone())),

        _ => None,
    }
}
