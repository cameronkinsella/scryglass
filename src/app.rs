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
//!
//! Animated GIFs are decoded into compact sub-rectangle frames and composited
//! at display time. Only one GPU texture is active per GIF at any moment.
//! Pre-fetched GIF decode data is cached so navigation to neighbor GIFs is
//! instant.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::time::Instant;
use iced::widget::image::{Allocation, Handle};
use iced::window;
use iced::{Element, Event, Subscription, Task, event, keyboard};

use crate::cache;
use crate::config::AppConfig;
use crate::gif::{self, DecodedGif, GifCanvas};
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
        /// GPU-allocated texture for the current image / current GIF frame.
        /// Once set, this is NEVER set to `None`. On navigation we keep
        /// the previous image visible until the new allocation arrives.
        current_allocation: Option<Allocation>,
        /// Pre-allocated textures for neighbor images. Holding these keeps
        /// iced's GPU texture memory alive so navigation is instant.
        _prefetch_allocations: Vec<Allocation>,
        /// True while waiting for the current image's allocation to arrive.
        /// Navigation is blocked until this clears.
        loading: bool,
        /// Which arrow key is currently held, and when the hold started.
        held_direction: Option<(Direction, Instant)>,
        /// Animated GIF state. `None` for static images.
        gif_state: Option<Box<GifState>>,
        /// Pre-decoded GIF data cache (keyed by path).
        /// Neighbor GIFs are decoded during prefetch so navigation is instant.
        gif_cache: HashMap<PathBuf, Arc<DecodedGif>>,
    },
}

/// State for an actively-animating GIF.
struct GifState {
    /// The decoded GIF data (shared with gif_cache via Arc).
    decoded: Arc<DecodedGif>,
    /// Canvas for compositing frames at display time.
    canvas: GifCanvas,
    /// Current frame index (circular).
    frame_index: usize,
    /// The active GPU allocation for the current composited frame.
    /// We hold this to keep the texture alive until the next frame.
    _frame_allocation: Option<Allocation>,
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
    /// A static image allocation completed (current or prefetch).
    ImageAllocated(PathBuf, Result<Allocation, cache::Error>),
    /// GIF decode completed (off-thread). Carries path and decoded data.
    GifDecoded(PathBuf, Result<Arc<DecodedGif>, String>),
    /// A GIF frame was allocated to GPU memory.
    GifFrameAllocated(PathBuf, Result<Allocation, cache::Error>),
    /// Timer tick to advance GIF animation.
    GifTick,
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
                let tasks =
                    load_current_and_prefetch(&nav, &HashMap::new(), app.config.prefetch_depth);
                app.state = AppState::Viewing {
                    nav,
                    current_allocation: None,
                    _prefetch_allocations: Vec::new(),
                    loading: true,
                    held_direction: None,
                    gif_state: None,
                    gif_cache: HashMap::new(),
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
            } else {
                // This is a prefetch. Hold the allocation to keep texture alive.
                _prefetch_allocations.push(allocation);
            }
            Task::none()
        }

        Message::ImageAllocated(_path, Err(_err)) => Task::none(),

        Message::GifDecoded(path, Ok(decoded)) => {
            let AppState::Viewing {
                nav,
                gif_cache,
                gif_state,
                loading,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            // Always cache the decoded data.
            gif_cache.insert(path.clone(), Arc::clone(&decoded));

            // If this is the current image, start displaying it.
            if nav.current() == path {
                return start_gif_display(decoded, &path, gif_state, loading);
            }

            Task::none()
        }

        Message::GifDecoded(_path, Err(_err)) => Task::none(),

        Message::GifFrameAllocated(path, Ok(allocation)) => {
            let AppState::Viewing {
                nav,
                current_allocation,
                loading,
                gif_state,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            if nav.current() != path {
                return Task::none();
            }

            let Some(gs) = gif_state.as_mut() else {
                return Task::none();
            };

            *current_allocation = Some(allocation.clone());
            gs._frame_allocation = Some(allocation);
            *loading = false;

            Task::none()
        }

        Message::GifFrameAllocated(_path, Err(_err)) => Task::none(),

        Message::GifTick => {
            let AppState::Viewing { nav, gif_state, .. } = &mut app.state else {
                return Task::none();
            };

            let Some(gs) = gif_state.as_mut() else {
                return Task::none();
            };

            let frame_count = gs.decoded.frames.len();
            if frame_count <= 1 {
                return Task::none();
            }

            // Apply disposal from the current frame, then advance.
            let current_frame = &gs.decoded.frames[gs.frame_index];
            gs.canvas.apply_disposal(current_frame);

            // Advance to next frame (circular).
            gs.frame_index = (gs.frame_index + 1) % frame_count;

            // Composite the new frame.
            let next_frame = &gs.decoded.frames[gs.frame_index];
            gs.canvas.composite_frame(next_frame);

            // Allocate the composited canvas to GPU.
            let pixels = gs.canvas.pixels().to_vec();
            let handle = Handle::from_rgba(gs.decoded.width, gs.decoded.height, pixels);
            let p = nav.current().to_path_buf();
            cache::allocate_handle(handle)
                .map(move |result| Message::GifFrameAllocated(p.clone(), result))
        }

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

/// Begin displaying a GIF from its decoded data.
///
/// Composites frame 0, fires an allocation task, and sets up the GIF state.
fn start_gif_display(
    decoded: Arc<DecodedGif>,
    path: &std::path::Path,
    gif_state: &mut Option<Box<GifState>>,
    loading: &mut bool,
) -> Task<Message> {
    let mut canvas = GifCanvas::new(decoded.width, decoded.height);
    let first_frame = &decoded.frames[0];
    canvas.composite_frame(first_frame);

    let pixels = canvas.pixels().to_vec();
    let handle = Handle::from_rgba(decoded.width, decoded.height, pixels);

    *gif_state = Some(Box::new(GifState {
        decoded,
        canvas,
        frame_index: 0,
        _frame_allocation: None,
    }));
    *loading = true;

    let p = path.to_path_buf();
    cache::allocate_handle(handle).map(move |result| Message::GifFrameAllocated(p.clone(), result))
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
        gif_state,
        gif_cache,
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
    // Clear GIF state since we're navigating away from any active GIF.
    *gif_state = None;

    // Clear old prefetch allocations. They'll be replaced by new ones.
    _prefetch_allocations.clear();

    // Prune the GIF cache to only keep current + neighbor GIFs.
    let current_path = nav.current().to_path_buf();
    let neighbors: std::collections::HashSet<PathBuf> = {
        let mut set = std::collections::HashSet::new();
        set.insert(current_path.clone());
        for p in nav.peek_around(app.config.prefetch_depth) {
            set.insert(p);
        }
        set
    };
    gif_cache.retain(|path, _| neighbors.contains(path));
    if gif::is_gif(&current_path)
        && let Some(decoded) = gif_cache.get(&current_path).cloned()
    {
        // GIF already decoded, start displaying immediately.
        let task = start_gif_display(decoded, &current_path, gif_state, loading);
        // Still fire prefetch for neighbors.
        let prefetch = prefetch_neighbors(nav, gif_cache, app.config.prefetch_depth);
        return Task::batch([task, prefetch]);
    }

    load_current_and_prefetch(nav, gif_cache, app.config.prefetch_depth)
}

/// Fire allocation/decode tasks for the current image and its neighbors.
fn load_current_and_prefetch(
    nav: &Nav,
    gif_cache: &HashMap<PathBuf, Arc<DecodedGif>>,
    depth: usize,
) -> Task<Message> {
    let current_path = nav.current().to_path_buf();

    let current_task = if gif::is_gif(&current_path) {
        // Decode GIF frames off the main thread.
        let p = current_path.clone();
        Task::perform(
            async move {
                match gif::decode_gif(&p) {
                    Ok(decoded) => (p, Ok(decoded)),
                    Err(e) => (p, Err(e.to_string())),
                }
            },
            |(path, result)| Message::GifDecoded(path, result),
        )
    } else {
        let p = current_path.clone();
        cache::allocate_path(&p).map(move |result| Message::ImageAllocated(p.clone(), result))
    };

    let prefetch = prefetch_neighbors(nav, gif_cache, depth);

    Task::batch([current_task, prefetch])
}

/// Fire prefetch tasks for neighbor images/GIFs.
fn prefetch_neighbors(
    nav: &Nav,
    gif_cache: &HashMap<PathBuf, Arc<DecodedGif>>,
    depth: usize,
) -> Task<Message> {
    let tasks: Vec<Task<Message>> = nav
        .peek_around(depth)
        .into_iter()
        .map(|p| {
            if gif::is_gif(&p) {
                // Only decode if not already cached.
                if gif_cache.contains_key(&p) {
                    return Task::none();
                }
                let p2 = p.clone();
                Task::perform(
                    async move {
                        match gif::decode_gif(&p2) {
                            Ok(decoded) => (p2, Ok(decoded)),
                            Err(e) => (p2, Err(e.to_string())),
                        }
                    },
                    |(path, result)| Message::GifDecoded(path, result),
                )
            } else {
                let p2 = p.clone();
                cache::allocate_path(&p)
                    .map(move |result| Message::ImageAllocated(p2.clone(), result))
            }
        })
        .collect();

    Task::batch(tasks)
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

/// Subscription: listens for keyboard and file-drop events, plus GIF animation ticks.
pub fn subscription(app: &App) -> Subscription<Message> {
    let events = event::listen_with(handle_event);

    // If viewing an animated GIF with multiple frames, tick at the current frame's delay.
    if let AppState::Viewing {
        gif_state: Some(gs),
        loading: false,
        ..
    } = &app.state
        && gs.decoded.frames.len() > 1
    {
        let delay = gs.decoded.frames[gs.frame_index].delay;
        let tick = iced::time::every(delay).map(|_| Message::GifTick);
        return Subscription::batch([events, tick]);
    }

    events
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
