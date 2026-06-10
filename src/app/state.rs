//! Session and viewer state.

use std::collections::HashSet;
use std::path::PathBuf;

use iced::time::Instant;
use iced::widget::image::Allocation;

use crate::gif::GifPlayer;
use crate::media::cache::ImageCache;
use crate::nav::Nav;

/// Whether the app is idle or actively viewing a directory of images.
pub enum Session {
    /// Waiting for a file drop or open.
    Empty,
    /// Actively viewing images.
    Viewing(Box<Viewer>),
}

/// A decoded image resident on the GPU, as stored in the cache.
#[derive(Debug, Clone)]
pub struct CachedImage {
    pub allocation: Allocation,
    /// True dimensions (post-orientation, pre-downscale) for zoom math.
    pub original_size: (u32, u32),
}

impl CachedImage {
    /// Approximate GPU memory cost in bytes (RGBA8).
    pub fn byte_cost(&self) -> usize {
        let size = self.allocation.size();
        size.width as usize * size.height as usize * 4
    }
}

/// What the image area is currently showing.
#[derive(Debug, Clone, Default)]
pub enum DisplayedImage {
    /// Nothing yet, first image still loading.
    #[default]
    None,
    /// The fully decoded image.
    Full {
        allocation: Allocation,
        original_size: (u32, u32),
    },
}

impl DisplayedImage {
    /// True image dimensions, if anything is displayed.
    pub fn original_size(&self) -> Option<(u32, u32)> {
        match self {
            DisplayedImage::None => None,
            DisplayedImage::Full { original_size, .. } => Some(*original_size),
        }
    }
}

/// All state tied to an open directory of images.
pub struct Viewer {
    pub nav: Nav,
    /// What the image area shows. Never reset to `None` during navigation,
    /// the old image stays visible until the new one is ready
    /// (flicker prevention).
    pub displayed: DisplayedImage,
    /// GPU-resident decoded images, keyed by path, with an LRU byte budget.
    /// Holding an `Allocation` keeps iced's texture alive, so cache hits
    /// render instantly.
    pub cache: ImageCache<CachedImage>,
    /// Paths with a load currently in flight, to avoid duplicate decodes.
    pub in_flight: HashSet<PathBuf>,
    /// When the current image's load started, if it isn't displayed yet.
    /// Drives the loading spinner (shown only after a grace period).
    pub pending_since: Option<Instant>,
    /// Which direction key is currently held, and when the hold started.
    pub held_direction: Option<(Direction, Instant)>,
    /// Animated GIF player that handles decode cache and animation.
    pub gif_player: GifPlayer,
    /// File size in bytes of the current image. `None` while the async
    /// metadata probe is in flight.
    pub current_file_size: Option<u64>,
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
    /// Fresh viewer for a newly scanned directory, with the first load and
    /// metadata probe pending.
    pub fn new(nav: Nav, gif_player: GifPlayer, cache_budget_bytes: usize) -> Self {
        Self {
            nav,
            displayed: DisplayedImage::None,
            cache: ImageCache::new(cache_budget_bytes),
            in_flight: HashSet::new(),
            pending_since: Some(Instant::now()),
            held_direction: None,
            gif_player,
            current_file_size: None,
            zoom: 1.0,
            manual_zoom: false,
            pan: (0.0, 0.0),
            drag: None,
        }
    }

    /// The paths that must stay cached: the current image plus the
    /// prefetch window around it.
    pub fn pinned_paths(&self, depth: usize) -> HashSet<PathBuf> {
        let mut pinned: HashSet<PathBuf> = self.nav.peek_around(depth).into_iter().collect();
        pinned.insert(self.nav.current().to_path_buf());
        pinned
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
