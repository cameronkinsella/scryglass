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

use std::path::PathBuf;
use std::time::Duration;

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::time::Instant;
use iced::widget::image::Allocation;
use iced::window;
use iced::{Element, Event, Subscription, Task, event, keyboard};

use crate::cache;
use crate::config::AppConfig;
use crate::nav::{self, Nav};
use crate::viewer;

/// How long the arrow key must be held before continuous scrolling begins.
/// A quick tap navigates exactly one image. OS key-repeat events arriving
/// before this threshold are ignored.
const HOLD_THRESHOLD: Duration = Duration::from_millis(300);

/// Application state: the single source of truth.
pub struct App {
    state: AppState,
    config: AppConfig,
}

enum AppState {
    /// Waiting for a file drop.
    Empty,
    /// Actively viewing images.
    Viewing {
        nav: Nav,
        /// GPU-allocated texture for the current image.
        /// Once set, this is NEVER set to `None`. On navigation we keep
        /// the previous image visible until the new allocation arrives.
        current_allocation: Option<Allocation>,
        /// Pre-allocated textures for neighbor images. Holding these keeps
        /// iced's GPU texture memory alive so navigation is instant.
        _prefetch_allocations: Vec<Allocation>,
        /// True while waiting for the current image's `ImageAllocated`.
        /// Navigation is blocked until this clears.
        loading: bool,
        /// Which arrow key is currently held, and when the hold started.
        /// Set on the initial `KeyPressed` (non-repeat), cleared on
        /// `KeyReleased`. OS key-repeat events only navigate after
        /// `HOLD_THRESHOLD` has elapsed since the initial press.
        held_direction: Option<(Direction, Instant)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A file was dropped onto the window.
    FileDropped(PathBuf),
    /// Directory scan completed.
    DirectoryScanned(PathBuf, Result<Vec<PathBuf>, String>),
    /// An image allocation completed (current or prefetch).
    ImageAllocated(PathBuf, Result<Allocation, cache::Error>),
    /// Navigate forward (arrow right initial press).
    Next,
    /// Navigate backward (arrow left initial press).
    Prev,
    /// Navigate forward (arrow right OS key-repeat).
    NextRepeat,
    /// Navigate backward (arrow left OS key-repeat).
    PrevRepeat,
    /// Arrow right released.
    NextReleased,
    /// Arrow left released.
    PrevReleased,
}

/// Boot function: creates the initial state. Called once by iced.
pub fn boot() -> App {
    let config = AppConfig::default();
    App {
        state: AppState::Empty,
        config,
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
        Message::FileDropped(path) => {
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

        Message::DirectoryScanned(start_file, Ok(files)) => match Nav::new(files, &start_file) {
            Ok(nav) => {
                let tasks = allocate_current_and_prefetch(&nav, app.config.prefetch_depth);
                app.state = AppState::Viewing {
                    nav,
                    current_allocation: None,
                    _prefetch_allocations: Vec::new(),
                    loading: true,
                    held_direction: None,
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
                // This is the current image, show it now.
                *current_allocation = Some(allocation);
                *loading = false;
                // No auto-advance here. Continuous scrolling is driven by
                // OS key-repeat events (NextRepeat / PrevRepeat).
            } else {
                // This is a prefetch. Hold the allocation to keep texture alive.
                _prefetch_allocations.push(allocation);
            }
            Task::none()
        }

        Message::ImageAllocated(_path, Err(_err)) => Task::none(),

        // --- Initial press (non-repeat): always navigate + record hold start ---
        Message::Next => {
            let AppState::Viewing {
                loading,
                held_direction,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            // Record when the key was first pressed.
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

            // Only scroll if we're past the hold threshold.
            let past_threshold = held_direction
                .map(|(_, pressed_at)| pressed_at.elapsed() >= HOLD_THRESHOLD)
                .unwrap_or(false);

            if !past_threshold || *loading {
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

            let past_threshold = held_direction
                .map(|(_, pressed_at)| pressed_at.elapsed() >= HOLD_THRESHOLD)
                .unwrap_or(false);

            if !past_threshold || *loading {
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
    }
}

/// Navigate to the next/prev image.
///
/// We do NOT clear `current_allocation` here. The previous image stays visible
/// until the new `ImageAllocated` message arrives. This prevents flicker.
fn navigate(app: &mut App, direction: Direction) -> Task<Message> {
    let AppState::Viewing {
        nav,
        _prefetch_allocations,
        loading,
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

    // Clear old prefetch allocations. They'll be replaced by new ones.
    _prefetch_allocations.clear();

    allocate_current_and_prefetch(nav, app.config.prefetch_depth)
}

/// Fire allocation tasks for the current image and its neighbors.
fn allocate_current_and_prefetch(nav: &Nav, depth: usize) -> Task<Message> {
    let current_path = nav.current().to_path_buf();
    let current_task = {
        let p = current_path.clone();
        cache::allocate_path(&p).map(move |result| Message::ImageAllocated(p.clone(), result))
    };

    let prefetch_tasks: Vec<Task<Message>> = nav
        .peek_around(depth)
        .into_iter()
        .map(|p| {
            let p2 = p.clone();
            cache::allocate_path(&p).map(move |result| Message::ImageAllocated(p2.clone(), result))
        })
        .collect();

    let mut all = vec![current_task];
    all.extend(prefetch_tasks);
    Task::batch(all)
}

/// View function: pure rendering, no side effects.
pub fn view(app: &App) -> Element<'_, Message> {
    match &app.state {
        AppState::Empty => viewer::drop_prompt(),
        AppState::Viewing {
            current_allocation, ..
        } => {
            if let Some(allocation) = current_allocation {
                viewer::image_viewer(allocation)
            } else {
                viewer::loading_prompt()
            }
        }
    }
}

/// Subscription: listens for keyboard and file-drop events.
pub fn subscription(_app: &App) -> Subscription<Message> {
    event::listen_with(handle_event)
}

fn handle_event(event: Event, _status: event::Status, _id: window::Id) -> Option<Message> {
    match event {
        // Initial press: navigate one image immediately.
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::ArrowRight),
            repeat: false,
            ..
        }) => Some(Message::Next),
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::ArrowLeft),
            repeat: false,
            ..
        }) => Some(Message::Prev),
        // OS key-repeat: continuous scrolling (throttled by hold threshold).
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::ArrowRight),
            repeat: true,
            ..
        }) => Some(Message::NextRepeat),
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::ArrowLeft),
            repeat: true,
            ..
        }) => Some(Message::PrevRepeat),
        // Key released, stop scrolling.
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(Named::ArrowRight),
            ..
        }) => Some(Message::NextReleased),
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(Named::ArrowLeft),
            ..
        }) => Some(Message::PrevReleased),
        Event::Window(window::Event::FileDropped(path)) => Some(Message::FileDropped(path)),
        _ => None,
    }
}
