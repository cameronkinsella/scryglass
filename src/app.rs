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
use iced::{Element, Event, Length, Size, Subscription, Task, event, keyboard, mouse};

use crate::cache;
use crate::config::{AppConfig, ZoomMode};
use crate::gif::{self, GifMessage, GifPlayer};
use crate::nav::{self, Nav};
use crate::widgets;
use crate::widgets::toolbar::{LayoutVisibility, OpenMenu};

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
    state: AppState,
    config: AppConfig,
    /// Which toolbar dropdown menu is open (if any).
    open_menu: Option<OpenMenu>,
    /// Current zoom mode.
    zoom_mode: ZoomMode,
    /// Size of the viewport (content area below toolbar, above footer).
    /// Updated on every window resize.
    viewport_size: Size,
    /// Last known cursor position (updated on every CursorMoved event).
    last_cursor_pos: iced::Point,
    /// Whether the toolbar is visible.
    show_toolbar: bool,
    /// Whether the filmstrip is visible.
    show_filmstrip: bool,
    /// Whether the navigation slider is visible.
    show_slider: bool,
    /// Whether the footer is visible.
    show_footer: bool,
    /// Last known window size (for recalculating viewport on layout toggles).
    window_size: Size,
    /// Context menu position (window coords). `Some` when visible.
    context_menu_pos: Option<iced::Point>,
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
        /// Current zoom factor (1.0 = 100%).
        zoom: f32,
        /// Whether the user has manually adjusted zoom (scroll wheel).
        manual_zoom: bool,
        /// Pan offset in logical pixels (applied when image overflows viewport).
        pan: (f32, f32),
        /// Mouse drag state for panning.
        drag: Option<DragState>,
    },
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    /// Mouse position when drag started.
    start: iced::Point,
    /// Pan offset when drag started.
    start_pan: (f32, f32),
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
    /// Toggle the Zoom dropdown menu.
    ToggleZoomMenu,
    /// Toggle the Layout dropdown menu.
    ToggleLayoutMenu,
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
    /// Set the zoom mode.
    SetZoomMode(ZoomMode),
    /// Scroll wheel zoom, delta lines Y.
    ScrollZoom(f32),
    /// Double-click: reset zoom to auto/opening state.
    ResetZoom,
    /// Mouse pressed on image area, begin drag.
    DragStart,
    /// Mouse moved during drag.
    DragMove(iced::Point),
    /// Mouse released, end drag.
    DragEnd,
    /// Window resized.
    WindowResized(Size),
    /// Slider dragged to an image index.
    SliderChanged(usize),
    /// Filmstrip thumbnail clicked.
    FilmstripClicked(usize),
    /// Toggle filmstrip visibility.
    ToggleFilmstrip,
    /// Toggle slider visibility.
    ToggleSlider,
    /// Toggle footer visibility.
    ToggleFooter,
    /// Vertical scroll over filmstrip, convert to horizontal scroll.
    FilmstripScroll(f32),
    /// Toggle toolbar visibility.
    ToggleToolbar,
    /// Show the context menu at the cursor position.
    ShowContextMenu,
    /// Dismiss the context menu.
    DismissContextMenu,
    /// Copy the current image to clipboard (as bitmap).
    CopyImage,
    /// Copy the full file path to clipboard.
    CopyFilePath,
    /// Copy just the filename to clipboard.
    CopyFilename,
    /// Open the folder containing the image in the native file explorer.
    OpenImageLocation,
    /// Open the native file properties dialog on the Details tab.
    ImageProperties,
}

/// Boot function: creates the initial state. Called once by iced.
pub fn boot() -> App {
    App {
        state: AppState::Empty,
        config: AppConfig::default(),
        open_menu: None,
        zoom_mode: ZoomMode::default(),
        viewport_size: Size::new(800.0, 600.0),
        last_cursor_pos: iced::Point::ORIGIN,
        show_toolbar: true,
        show_filmstrip: true,
        show_slider: true,
        show_footer: true,
        window_size: Size::new(800.0, 600.0),
        context_menu_pos: None,
    }
}

/// Title function: returns the window title based on current state.
///
/// When the footer is hidden, the title bar includes the info that
/// would normally appear in the footer: file index, zoom, dimensions,
/// and file size.
pub fn title(app: &App) -> String {
    match &app.state {
        AppState::Empty => String::new(),
        AppState::Viewing {
            nav,
            current_allocation,
            current_file_size,
            zoom,
            ..
        } => {
            let filename = nav
                .current()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            if app.show_footer {
                return filename;
            }

            let position = nav.position_label();
            let zoom_pct = (*zoom * 100.0).round() as u32;

            let dims = current_allocation
                .as_ref()
                .map(|a| {
                    let s = a.size();
                    widgets::format_dimensions(s.width, s.height)
                })
                .unwrap_or_default();

            let size = widgets::format_file_size(*current_file_size);

            format!("{filename}  |  {position}  |  {zoom_pct}%  |  {dims}  |  {size}")
        }
    }
}

/// Compute the "opening" zoom factor for Auto mode.
/// 100% if it fits, otherwise shrink-to-fit. Never scale up.
fn auto_zoom(img_w: u32, img_h: u32, vp: Size) -> f32 {
    if img_w == 0 || img_h == 0 {
        return 1.0;
    }
    let fit_w = vp.width / img_w as f32;
    let fit_h = vp.height / img_h as f32;
    let fit = fit_w.min(fit_h);
    fit.min(1.0)
}

/// Compute zoom factor for a given ZoomMode.
fn compute_zoom(mode: ZoomMode, img_w: u32, img_h: u32, vp: Size) -> f32 {
    if img_w == 0 || img_h == 0 {
        return 1.0;
    }
    match mode {
        ZoomMode::Auto | ZoomMode::LockZoomRatio => auto_zoom(img_w, img_h, vp),
        ZoomMode::ScaleToWidth => vp.width / img_w as f32,
        ZoomMode::ScaleToHeight => vp.height / img_h as f32,
        ZoomMode::ScaleToFit => {
            let fit_w = vp.width / img_w as f32;
            let fit_h = vp.height / img_h as f32;
            fit_w.min(fit_h)
        }
        ZoomMode::ScaleToFill => {
            let fit_w = vp.width / img_w as f32;
            let fit_h = vp.height / img_h as f32;
            fit_w.max(fit_h)
        }
    }
}

/// Clamp pan offset so the image doesn't scroll past edges.
fn clamp_pan(pan: (f32, f32), img_w: f32, img_h: f32, vp: Size) -> (f32, f32) {
    let excess_w = (img_w - vp.width).max(0.0) / 2.0;
    let excess_h = (img_h - vp.height).max(0.0) / 2.0;
    let x = pan.0.clamp(-excess_w, excess_w);
    let y = pan.1.clamp(-excess_h, excess_h);
    (x, y)
}

/// Recalculate the viewport size based on window size and visible chrome.
fn recalc_viewport(app: &mut App) {
    let mut chrome_height: f32 = if app.show_toolbar { TOOLBAR_HEIGHT } else { 0.0 };
    if app.show_filmstrip {
        chrome_height += 72.0; // filmstrip + padding
    }
    if app.show_slider {
        chrome_height += 28.0; // slider + padding
    }
    if app.show_footer {
        chrome_height += 25.0; // footer
    }
    app.viewport_size = Size::new(
        app.window_size.width,
        (app.window_size.height - chrome_height).max(1.0),
    );
}

/// Returns true if the message is related to menu interaction
/// (opening/closing menus, selecting menu items, or passive events
/// that shouldn't dismiss menus like cursor moves and window resizes).
fn is_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ToggleFileMenu
            | Message::ToggleZoomMenu
            | Message::ToggleLayoutMenu
            | Message::DismissOverlay
            | Message::OpenFile
            | Message::CloseFile
            | Message::Quit
            | Message::SetZoomMode(_)
            | Message::ToggleFilmstrip
            | Message::ToggleSlider
            | Message::ToggleFooter
            | Message::ToggleToolbar
            // Context menu messages:
            | Message::ShowContextMenu
            | Message::DismissContextMenu
            | Message::CopyImage
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            // Passive events that shouldn't dismiss menus:
            | Message::DragMove(_)
            | Message::DragEnd
            | Message::WindowResized(_)
            | Message::ImageAllocated(_, _)
            | Message::Gif(_)
            | Message::DirectoryScanned(_, _)
            | Message::FileDialogResult(_)
            | Message::NextReleased
            | Message::PrevReleased
    )
}

/// Returns true if the message belongs to the context menu flow.
fn is_context_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ShowContextMenu
            | Message::DismissContextMenu
            | Message::CopyImage
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            | Message::ToggleToolbar
            // Passive events:
            | Message::DragMove(_)
            | Message::WindowResized(_)
            | Message::ImageAllocated(_, _)
            | Message::Gif(_)
            | Message::DirectoryScanned(_, _)
            | Message::FileDialogResult(_)
            | Message::NextReleased
            | Message::PrevReleased
    )
}

/// Update function: handles messages and mutates state.
pub fn update(app: &mut App, message: Message) -> Task<Message> {
    // Auto-dismiss any open dropdown when the user interacts outside the menu.
    if app.open_menu.is_some() && !is_menu_message(&message) {
        app.open_menu = None;
    }

    // Auto-dismiss context menu on any non-context-menu interaction.
    if app.context_menu_pos.is_some() && !is_context_menu_message(&message) {
        app.context_menu_pos = None;
    }

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
                    zoom: 1.0,
                    manual_zoom: false,
                    pan: (0.0, 0.0),
                    drag: None,
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
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            if nav.current() == path {
                let size = allocation.size();
                if !*manual_zoom || app.zoom_mode != ZoomMode::LockZoomRatio {
                    *zoom = compute_zoom(app.zoom_mode, size.width, size.height, app.viewport_size);
                }
                *pan = (0.0, 0.0);
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
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            let is_first_frame = current_allocation.is_none()
                || (*loading && matches!(&gif_msg, GifMessage::FrameAllocated(..)));

            let (task, allocation) = gif_player.update(gif_msg, nav.current());

            if let Some(alloc) = allocation {
                if is_first_frame && (!*manual_zoom || app.zoom_mode != ZoomMode::LockZoomRatio) {
                    let size = alloc.size();
                    *zoom = compute_zoom(app.zoom_mode, size.width, size.height, app.viewport_size);
                    *pan = (0.0, 0.0);
                }
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
            app.open_menu = if app.open_menu == Some(OpenMenu::File) {
                None
            } else {
                Some(OpenMenu::File)
            };
            Task::none()
        }

        Message::ToggleZoomMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Zoom) {
                None
            } else {
                Some(OpenMenu::Zoom)
            };
            Task::none()
        }

        Message::ToggleLayoutMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Layout) {
                None
            } else {
                Some(OpenMenu::Layout)
            };
            Task::none()
        }

        Message::DismissOverlay => {
            app.open_menu = None;
            Task::none()
        }

        // --- File menu actions ---
        Message::OpenFile => {
            app.open_menu = None;
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
            app.open_menu = None;
            app.state = AppState::Empty;
            Task::none()
        }

        Message::Quit => iced::exit(),

        // --- Zoom mode ---
        Message::SetZoomMode(mode) => {
            app.open_menu = None;
            app.zoom_mode = mode;

            let AppState::Viewing {
                current_allocation,
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            *manual_zoom = false;

            if let Some(alloc) = current_allocation {
                let size = alloc.size();
                *zoom = compute_zoom(mode, size.width, size.height, app.viewport_size);
                let img_w = size.width as f32 * *zoom;
                let img_h = size.height as f32 * *zoom;
                *pan = clamp_pan(*pan, img_w, img_h, app.viewport_size);
            }
            Task::none()
        }

        // --- Scroll-wheel zoom (toward cursor) ---
        Message::ScrollZoom(delta_y) => {
            let AppState::Viewing {
                current_allocation,
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            let old_zoom = *zoom;
            let factor = if delta_y > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            *zoom = (old_zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            *manual_zoom = true;

            // Adjust pan so the source pixel under the cursor stays fixed.
            // cursor_in_viewport = window cursor pos minus toolbar height.
            // d = cursor offset from viewport center (in logical pixels).
            // ratio = new_zoom / old_zoom.
            // pan_new = d * (1 - ratio) + pan_old * ratio
            let ratio = *zoom / old_zoom;
            let toolbar_offset = if app.show_toolbar { TOOLBAR_HEIGHT } else { 0.0 };
            let d_x = app.last_cursor_pos.x - app.viewport_size.width / 2.0;
            let d_y = app.last_cursor_pos.y - toolbar_offset - app.viewport_size.height / 2.0;
            *pan = (
                d_x * (1.0 - ratio) + pan.0 * ratio,
                d_y * (1.0 - ratio) + pan.1 * ratio,
            );

            if let Some(alloc) = current_allocation {
                let size = alloc.size();
                let img_w = size.width as f32 * *zoom;
                let img_h = size.height as f32 * *zoom;
                *pan = clamp_pan(*pan, img_w, img_h, app.viewport_size);
            }
            Task::none()
        }

        // --- Double-click: reset zoom ---
        Message::ResetZoom => {
            let AppState::Viewing {
                current_allocation,
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            *manual_zoom = false;
            if let Some(alloc) = current_allocation {
                let size = alloc.size();
                *zoom = compute_zoom(app.zoom_mode, size.width, size.height, app.viewport_size);
            }
            *pan = (0.0, 0.0);
            Task::none()
        }

        // --- Drag to pan ---
        Message::DragStart => {
            if let AppState::Viewing { drag, pan, .. } = &mut app.state {
                *drag = Some(DragState {
                    start: app.last_cursor_pos,
                    start_pan: *pan,
                });
            }
            Task::none()
        }

        Message::DragMove(pos) => {
            app.last_cursor_pos = pos;

            let AppState::Viewing {
                drag,
                pan,
                zoom,
                current_allocation,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            if let Some(ds) = drag {
                let dx = pos.x - ds.start.x;
                let dy = pos.y - ds.start.y;
                let new_pan = (ds.start_pan.0 + dx, ds.start_pan.1 + dy);

                if let Some(alloc) = current_allocation {
                    let size = alloc.size();
                    let img_w = size.width as f32 * *zoom;
                    let img_h = size.height as f32 * *zoom;
                    *pan = clamp_pan(new_pan, img_w, img_h, app.viewport_size);
                }
            }
            Task::none()
        }

        Message::DragEnd => {
            if let AppState::Viewing { drag, .. } = &mut app.state {
                *drag = None;
            }
            Task::none()
        }

        // --- Window resized ---
        Message::WindowResized(size) => {
            app.window_size = size;
            recalc_viewport(app);

            let AppState::Viewing {
                current_allocation,
                zoom,
                manual_zoom,
                pan,
                ..
            } = &mut app.state
            else {
                return Task::none();
            };

            if !*manual_zoom && let Some(alloc) = current_allocation {
                let s = alloc.size();
                *zoom = compute_zoom(app.zoom_mode, s.width, s.height, app.viewport_size);
            }

            if let Some(alloc) = current_allocation {
                let s = alloc.size();
                let img_w = s.width as f32 * *zoom;
                let img_h = s.height as f32 * *zoom;
                *pan = clamp_pan(*pan, img_w, img_h, app.viewport_size);
            }
            Task::none()
        }

        // --- Slider and filmstrip visibility ---
        Message::ToggleFilmstrip => {
            app.show_filmstrip = !app.show_filmstrip;
            recalc_viewport(app);
            Task::none()
        }

        Message::ToggleSlider => {
            app.show_slider = !app.show_slider;
            recalc_viewport(app);
            Task::none()
        }

        Message::ToggleFooter => {
            app.show_footer = !app.show_footer;
            recalc_viewport(app);
            Task::none()
        }

        Message::FilmstripScroll(delta_y) => {
            // Convert vertical scroll delta to horizontal scroll on the filmstrip.
            let offset = iced::widget::scrollable::AbsoluteOffset {
                x: -delta_y * 60.0,
                y: 0.0,
            };
            iced::widget::operation::scroll_by(widgets::filmstrip::filmstrip_id(), offset)
        }

        Message::SliderChanged(index) | Message::FilmstripClicked(index) => {
            navigate_to_index(app, index)
        }

        // --- Toolbar visibility ---
        Message::ToggleToolbar => {
            app.show_toolbar = !app.show_toolbar;
            app.context_menu_pos = None;
            recalc_viewport(app);
            Task::none()
        }

        // --- Context menu ---
        Message::ShowContextMenu => {
            app.context_menu_pos = Some(app.last_cursor_pos);
            Task::none()
        }

        Message::DismissContextMenu => {
            app.context_menu_pos = None;
            Task::none()
        }

        Message::CopyImage => {
            app.context_menu_pos = None;
            let AppState::Viewing { nav, .. } = &app.state else {
                return Task::none();
            };
            let path = nav.current().to_path_buf();
            Task::perform(
                async move { crate::platform::copy_image_to_clipboard(&path) },
                |_| Message::DismissOverlay, // no-op follow-up
            )
        }

        Message::CopyFilePath => {
            app.context_menu_pos = None;
            let AppState::Viewing { nav, .. } = &app.state else {
                return Task::none();
            };
            let path_str = nav.current().to_string_lossy().to_string();
            iced::clipboard::write(path_str)
        }

        Message::CopyFilename => {
            app.context_menu_pos = None;
            let AppState::Viewing { nav, .. } = &app.state else {
                return Task::none();
            };
            let name = nav
                .current()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            iced::clipboard::write(name)
        }

        Message::OpenImageLocation => {
            app.context_menu_pos = None;
            let AppState::Viewing { nav, .. } = &app.state else {
                return Task::none();
            };
            let path = nav.current().to_path_buf();
            crate::platform::reveal_in_file_manager(&path);
            Task::none()
        }

        Message::ImageProperties => {
            app.context_menu_pos = None;
            let AppState::Viewing { nav, .. } = &app.state else {
                return Task::none();
            };
            let path = nav.current().to_path_buf();
            crate::platform::show_properties(&path);
            Task::none()
        }
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
        manual_zoom,
        pan,
        drag,
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
    *drag = None;

    // Reset pan on navigation. Zoom is preserved only in LockZoomRatio mode.
    // Don't change zoom here. The previous image stays visible until the new
    // allocation arrives (flicker prevention), so keep the old zoom to avoid a
    // brief flash at the wrong scale. The correct zoom will be set in
    // ImageAllocated / GifMessage::FrameAllocated.
    *pan = (0.0, 0.0);
    if app.zoom_mode != ZoomMode::LockZoomRatio {
        *manual_zoom = false;
    }

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

/// Jump to a specific image index (slider / filmstrip click).
fn navigate_to_index(app: &mut App, index: usize) -> Task<Message> {
    let AppState::Viewing {
        nav,
        _prefetch_allocations,
        loading,
        gif_player,
        current_file_size,
        manual_zoom,
        pan,
        drag,
        ..
    } = &mut app.state
    else {
        return Task::none();
    };

    // Don't navigate if already at this index.
    if nav.cursor() == index {
        return Task::none();
    }

    nav.set_cursor(index);

    *loading = true;
    gif_player.stop();
    _prefetch_allocations.clear();
    *drag = None;
    *pan = (0.0, 0.0);
    if app.zoom_mode != ZoomMode::LockZoomRatio {
        *manual_zoom = false;
    }

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
    let layout_vis = LayoutVisibility {
        show_filmstrip: app.show_filmstrip,
        show_slider: app.show_slider,
        show_footer: app.show_footer,
    };

    let content = match &app.state {
        AppState::Empty => widgets::image_display::drop_prompt(),
        AppState::Viewing {
            nav,
            current_allocation,
            current_file_size,
            zoom,
            pan,
            ..
        } => match current_allocation {
            Some(allocation) => {
                let size = allocation.size();
                let zoom_pct = (*zoom * 100.0).round() as u32;

                let image_view = widgets::image_display::image_display(
                    allocation,
                    *zoom,
                    *pan,
                    (app.viewport_size.width, app.viewport_size.height),
                );

                // Wrap image area in mouse_area for scroll, drag, double-click, and right-click.
                let interactive = mouse_area(image_view)
                    .on_press(Message::DragStart)
                    .on_right_press(Message::ShowContextMenu)
                    .on_scroll(|delta| {
                        let y = match delta {
                            mouse::ScrollDelta::Lines { y, .. } => y,
                            mouse::ScrollDelta::Pixels { y, .. } => {
                                if y > 0.0 {
                                    1.0
                                } else if y < 0.0 {
                                    -1.0
                                } else {
                                    0.0
                                }
                            }
                        };
                        Message::ScrollZoom(y)
                    })
                    .on_double_click(Message::ResetZoom);

                // Build the bottom section: filmstrip, slider, footer (each optional).
                let mut col = column![interactive];

                if app.show_filmstrip {
                    col = col.push(widgets::filmstrip::filmstrip(nav.files(), nav.cursor()));
                }
                if app.show_slider {
                    col = col.push(widgets::nav_slider::nav_slider(nav.cursor(), nav.len()));
                }
                if app.show_footer {
                    let footer = widgets::footer::footer(
                        &widgets::format_dimensions(size.width, size.height),
                        &widgets::format_file_size(*current_file_size),
                        zoom_pct,
                        &nav.position_label(),
                    );
                    col = col.push(footer);
                }

                col.into()
            }
            None => widgets::image_display::loading_prompt(),
        },
    };

    // Main layout: toolbar on top (if visible), then content fills remaining space.
    // Always use Stack so the widget tree structure is stable. This
    // prevents iced from losing internal widget state (e.g. filmstrip
    // scroll position) when toggling menus.

    // Build the toolbar dropdown overlay (or invisible placeholder).
    let toolbar_overlay: Element<'_, Message> = if let Some(dropdown) =
        widgets::toolbar::dropdown(app.open_menu, app.zoom_mode, layout_vis)
    {
        column![dropdown]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        column![].width(Length::Fill).height(Length::Fill).into()
    };

    // Build the context menu overlay (or invisible placeholder).
    // The context menu is positioned inside the stacked area (below toolbar),
    // but pos is in window coordinates, so subtract toolbar height.
    let ctx_overlay: Element<'_, Message> = if let Some(pos) = app.context_menu_pos {
        let toolbar_offset = if app.show_toolbar { TOOLBAR_HEIGHT } else { 0.0 };
        let adjusted_pos = iced::Point::new(pos.x, pos.y - toolbar_offset);
        widgets::context_menu::context_menu(adjusted_pos, app.show_toolbar)
    } else {
        column![].width(Length::Fill).height(Length::Fill).into()
    };

    let stacked = Stack::with_children(vec![content, toolbar_overlay, ctx_overlay]);

    let mut page = column![]
        .width(Length::Fill)
        .height(Length::Fill);

    if app.show_toolbar {
        page = page.push(widgets::toolbar::menu_bar(app.open_menu));
    }
    page = page.push(stacked);

    if app.context_menu_pos.is_some() {
        mouse_area(page)
            .on_press(Message::DismissContextMenu)
            .on_right_press(Message::DismissContextMenu)
            .into()
    } else if app.open_menu.is_some() {
        mouse_area(page)
            .on_press(Message::DismissOverlay)
            .on_right_press(Message::DismissOverlay)
            .into()
    } else {
        mouse_area(page).into()
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

    const VP: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    // --- auto_zoom ---

    #[test]
    fn auto_zoom_returns_one_when_image_fits() {
        assert_eq!(auto_zoom(400, 300, VP), 1.0);
    }

    #[test]
    fn auto_zoom_returns_one_at_exact_viewport_size() {
        assert_eq!(auto_zoom(800, 600, VP), 1.0);
    }

    #[test]
    fn auto_zoom_shrinks_to_fit_width() {
        // 1600 wide in an 800 viewport → 0.5, height fits at that scale.
        assert_eq!(auto_zoom(1600, 600, VP), 0.5);
    }

    #[test]
    fn auto_zoom_shrinks_to_fit_height() {
        // 1200 tall in a 600 viewport → 0.5.
        assert_eq!(auto_zoom(800, 1200, VP), 0.5);
    }

    #[test]
    fn auto_zoom_uses_most_constrained_axis() {
        // fit_w = 0.5, fit_h = 0.25 → 0.25.
        assert_eq!(auto_zoom(1600, 2400, VP), 0.25);
    }

    #[test]
    fn auto_zoom_zero_dimension_returns_one() {
        assert_eq!(auto_zoom(0, 100, VP), 1.0);
        assert_eq!(auto_zoom(100, 0, VP), 1.0);
    }

    // --- compute_zoom ---

    #[test]
    fn compute_zoom_auto_never_scales_up() {
        assert_eq!(compute_zoom(ZoomMode::Auto, 400, 300, VP), 1.0);
    }

    #[test]
    fn compute_zoom_lock_ratio_matches_auto_on_open() {
        assert_eq!(
            compute_zoom(ZoomMode::LockZoomRatio, 1600, 600, VP),
            compute_zoom(ZoomMode::Auto, 1600, 600, VP),
        );
    }

    #[test]
    fn compute_zoom_scale_to_width_fills_width() {
        // Scales up: 400 wide → 800 viewport = 2.0.
        assert_eq!(compute_zoom(ZoomMode::ScaleToWidth, 400, 300, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_height_fills_height() {
        // 300 tall → 600 viewport = 2.0.
        assert_eq!(compute_zoom(ZoomMode::ScaleToHeight, 400, 300, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_fit_uses_min_axis() {
        // fit_w = 2.0, fit_h = 6.0 → 2.0 (no overflow).
        assert_eq!(compute_zoom(ZoomMode::ScaleToFit, 400, 100, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_fill_uses_max_axis() {
        // fit_w = 2.0, fit_h = 6.0 → 6.0 (width overflows).
        assert_eq!(compute_zoom(ZoomMode::ScaleToFill, 400, 100, VP), 6.0);
    }

    #[test]
    fn compute_zoom_zero_dimension_returns_one() {
        assert_eq!(compute_zoom(ZoomMode::ScaleToFill, 0, 100, VP), 1.0);
    }

    // --- clamp_pan ---

    #[test]
    fn clamp_pan_centers_image_smaller_than_viewport() {
        assert_eq!(clamp_pan((50.0, -30.0), 400.0, 300.0, VP), (0.0, 0.0));
    }

    #[test]
    fn clamp_pan_limits_to_half_the_excess() {
        // Image 1000×800 in 800×600: excess/2 = (100, 100).
        assert_eq!(clamp_pan((500.0, -500.0), 1000.0, 800.0, VP), (100.0, -100.0));
    }

    #[test]
    fn clamp_pan_keeps_in_bounds_pan_unchanged() {
        assert_eq!(clamp_pan((50.0, -50.0), 1000.0, 800.0, VP), (50.0, -50.0));
    }

    #[test]
    fn clamp_pan_clamps_one_axis_independently() {
        // Only width overflows: y is always forced to 0.
        assert_eq!(clamp_pan((500.0, 40.0), 1000.0, 300.0, VP), (100.0, 0.0));
    }
}
