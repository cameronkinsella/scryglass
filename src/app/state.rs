//! Session and viewer state.

use iced::time::Instant;
use iced::widget::image::Allocation;

use crate::gif::GifPlayer;
use crate::nav::Nav;

/// Whether the app is idle or actively viewing a directory of images.
pub enum Session {
    /// Waiting for a file drop or open.
    Empty,
    /// Actively viewing images.
    Viewing(Viewer),
}

/// All state tied to an open directory of images.
pub struct Viewer {
    pub nav: Nav,
    /// GPU-allocated texture for the current image / current GIF frame.
    /// Once set, this is NEVER set back to `None`. The old image stays
    /// visible until the next allocation arrives (flicker prevention).
    pub current_allocation: Option<Allocation>,
    /// Textures for neighbor images. Never read directly. Holding an
    /// `Allocation` keeps iced's GPU texture alive so that navigating to
    /// a neighbor renders instantly.
    pub prefetch_allocations: Vec<Allocation>,
    /// True while waiting for the current image's allocation.
    pub loading: bool,
    /// Which direction key is currently held, and when the hold started.
    pub held_direction: Option<(Direction, Instant)>,
    /// Animated GIF player that handles decode cache and animation.
    pub gif_player: GifPlayer,
    /// Cached file size in bytes of the current image (set on load).
    pub current_file_size: u64,
    /// Current zoom factor (1.0 = 100%).
    pub zoom: f32,
    /// Whether the user has manually adjusted zoom (scroll wheel).
    pub manual_zoom: bool,
    /// Pan offset in logical pixels (applied when image overflows viewport).
    pub pan: (f32, f32),
    /// Mouse drag state for panning.
    pub drag: Option<DragState>,
}

impl Viewer {
    /// Fresh viewer for a newly scanned directory, with the first load pending.
    pub fn new(nav: Nav, gif_player: GifPlayer, file_size: u64) -> Self {
        Self {
            nav,
            current_allocation: None,
            prefetch_allocations: Vec::new(),
            loading: true,
            held_direction: None,
            gif_player,
            current_file_size: file_size,
            zoom: 1.0,
            manual_zoom: false,
            pan: (0.0, 0.0),
            drag: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DragState {
    /// Mouse position when drag started.
    pub start: iced::Point,
    /// Pan offset when drag started.
    pub start_pan: (f32, f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}
