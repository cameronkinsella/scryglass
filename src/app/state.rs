//! Session and viewer state.

use std::collections::HashSet;
use std::path::PathBuf;

use iced::time::Instant;
use iced::widget::image::{Allocation, Handle};

use crate::anim::AnimPlayer;
use crate::media::cache::ImageCache;
use crate::media::pipeline::Source;
use crate::nav::Nav;

/// Thumbnail cache budget. Thumbs are ~256 KB each, so this holds 500+.
const THUMB_BUDGET_BYTES: usize = 128 * 1024 * 1024;

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

/// A small preview texture, used by the filmstrip and as the blurred
/// placeholder while the full image decodes.
#[derive(Debug, Clone)]
pub struct Thumb {
    pub handle: Handle,
    /// Thumbnail texture dimensions.
    pub size: (u32, u32),
    /// True dimensions of the image this previews. Zoom math runs on
    /// these so the placeholder's geometry matches the full image exactly.
    pub original_size: (u32, u32),
}

impl Thumb {
    /// Approximate memory cost in bytes (RGBA8).
    pub fn byte_cost(&self) -> usize {
        self.size.0 as usize * self.size.1 as usize * 4
    }
}

/// A finished pipeline load: media ready to show plus its derived thumbnail.
#[derive(Debug, Clone)]
pub enum LoadedMedia {
    /// A still image, already uploaded to the GPU.
    Static {
        image: CachedImage,
        thumb: Option<Thumb>,
    },
    /// A decoded animation, played frame-by-frame by the [`AnimPlayer`].
    Animated {
        anim: std::sync::Arc<crate::media::animation::AnimatedImage>,
        thumb: Option<Thumb>,
    },
}

/// What the image area is currently showing.
#[derive(Debug, Clone, Default)]
pub enum DisplayedImage {
    /// Nothing yet, first image still loading.
    #[default]
    None,
    /// A blurred low-res stand-in while the full image decodes.
    Placeholder(Thumb),
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
            DisplayedImage::Placeholder(thumb) => Some(thumb.original_size),
            DisplayedImage::Full { original_size, .. } => Some(*original_size),
        }
    }
}

/// All state tied to an open directory (or archive) of images.
pub struct Viewer {
    pub nav: Nav,
    /// Where this session's bytes come from (filesystem or an archive).
    pub source: Source,
    /// What the image area shows. Never reset to `None` during navigation,
    /// the old image stays visible until the new one is ready
    /// (flicker prevention).
    pub displayed: DisplayedImage,
    /// GPU-resident decoded images, keyed by path, with an LRU byte budget.
    /// Holding an `Allocation` keeps iced's texture alive, so cache hits
    /// render instantly.
    pub cache: ImageCache<CachedImage>,
    /// Small previews for placeholders and the filmstrip.
    pub thumbs: ImageCache<Thumb>,
    /// Paths with a full load currently in flight, to avoid duplicate decodes.
    pub in_flight: HashSet<PathBuf>,
    /// Paths with a thumbnail probe in flight.
    pub in_flight_thumbs: HashSet<PathBuf>,
    /// Paths whose background thumbnail attempt failed (corrupt or
    /// undecodable), never re-picked by the thumbnailer.
    pub failed_thumbs: HashSet<PathBuf>,
    /// Which file the image area currently shows (full or placeholder).
    /// `None` until the first image appears.
    pub displayed_path: Option<PathBuf>,
    /// When the current image's load started, if it isn't displayed yet.
    /// Drives the loading spinner (shown only after a grace period).
    pub pending_since: Option<Instant>,
    /// A navigation waiting for its target to become displayable. The
    /// cursor (and with it title, slider, filmstrip) does not move until
    /// the target has at least a blurred placeholder, so the screen never
    /// goes empty and never shows the wrong image. Further navigation
    /// requests are dropped while one is pending.
    pub pending_nav: Option<usize>,
    /// An active slider drag. The thumb follows the hand freely, the
    /// display live-follows through loaded files and the fallback bubble
    /// covers cold ones. Committed on release.
    pub slider_drag: Option<SliderDrag>,
    /// Which direction key is currently held, and when the hold started.
    pub held_direction: Option<(Direction, Instant)>,
    /// Animated GIF player that handles decode cache and animation.
    pub anim_player: AnimPlayer,
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
    /// Filmstrip scroll offset in logical pixels. Drives virtualization.
    pub filmstrip_scroll_x: f32,
    /// EXIF fields for the info panel, tagged with the file they describe.
    pub exif: Option<(PathBuf, Vec<(String, String)>)>,
    /// Desired view rotation in quarter turns clockwise (0-3).
    /// Non-destructive, reset when navigating to another image.
    pub rotation: u8,
    /// Rotation currently baked into the displayed texture. When this
    /// trails `rotation`, a rotate task is producing the next texture.
    pub displayed_rotation: u8,
}

impl Viewer {
    /// Fresh viewer for a newly scanned directory or archive, with the
    /// first load and metadata probe pending.
    pub fn new(
        nav: Nav,
        source: Source,
        anim_player: AnimPlayer,
        cache_budget_bytes: usize,
    ) -> Self {
        Self {
            nav,
            source,
            displayed: DisplayedImage::None,
            cache: ImageCache::new(cache_budget_bytes),
            thumbs: ImageCache::new(THUMB_BUDGET_BYTES),
            in_flight: HashSet::new(),
            in_flight_thumbs: HashSet::new(),
            failed_thumbs: HashSet::new(),
            displayed_path: None,
            pending_since: Some(Instant::now()),
            pending_nav: None,
            slider_drag: None,
            held_direction: None,
            anim_player,
            current_file_size: None,
            zoom: 1.0,
            manual_zoom: false,
            pan: (0.0, 0.0),
            drag: None,
            filmstrip_scroll_x: 0.0,
            exif: None,
            rotation: 0,
            displayed_rotation: 0,
        }
    }

    /// The paths that must stay cached: the current image plus the
    /// prefetch window around it.
    pub fn pinned_paths(&self, depth: usize) -> HashSet<PathBuf> {
        let mut pinned: HashSet<PathBuf> = self.nav.peek_around(depth).into_iter().collect();
        pinned.insert(self.nav.current().to_path_buf());
        pinned
    }

    /// True when this session navigates real files (not archive entries).
    #[allow(dead_code)] // file operations are filesystem-only
    pub fn is_fs(&self) -> bool {
        matches!(self.source, Source::Fs)
    }

    /// Whether anything can be put on screen for `path` right now:
    /// a decoded image, a thumbnail (blur), or a cached GIF.
    pub fn displayable(&self, path: &std::path::Path) -> bool {
        self.cache.contains(path) || self.thumbs.contains(path) || self.anim_player.has_cached(path)
    }

    /// The next file the background thumbnailer should work on: scans
    /// forward from the cursor (wrapping) for a file with no thumbnail,
    /// none in flight, and no full load underway (those yield a thumbnail
    /// as a by-product).
    pub fn next_unthumbed(&self) -> Option<PathBuf> {
        let files = self.nav.files();
        let len = files.len();
        let start = self.nav.cursor();
        (0..len)
            .map(|i| &files[(start + i) % len])
            .find(|p| {
                !self.thumbs.contains(p)
                    && !self.in_flight_thumbs.contains(*p)
                    && !self.failed_thumbs.contains(*p)
                    && !self.in_flight.contains(*p)
            })
            .cloned()
    }

    /// The on-disk file behind the current image: the file itself, or the
    /// archive containing it. Used by shell integration (reveal, properties).
    pub fn current_disk_path(&self) -> PathBuf {
        match &self.source {
            Source::Fs => self.nav.current().to_path_buf(),
            Source::Archive(index) => index.archive_path.clone(),
        }
    }
}

/// State of an in-progress slider drag.
#[derive(Debug, Clone, Copy)]
pub struct SliderDrag {
    /// The index under the user's hand.
    pub target: usize,
    /// Whether the fallback bubble has been triggered. Sticky: once true,
    /// it stays for the rest of the drag so it never flickers in and out
    /// across warm/cold boundaries.
    pub bubble: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_viewer(names: &[&str], cursor: usize) -> Viewer {
        let files: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
        let start = files[cursor].clone();
        let nav = Nav::new(files, &start).unwrap();
        Viewer::new(nav, Source::Fs, AnimPlayer::new(), 1024)
    }

    #[test]
    fn next_unthumbed_scans_forward_from_cursor_and_wraps() {
        let viewer = test_viewer(&["a.png", "b.png", "c.png"], 1);
        assert_eq!(viewer.next_unthumbed(), Some(PathBuf::from("b.png")));
    }

    #[test]
    fn next_unthumbed_skips_done_in_flight_and_failed() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png", "d.png"], 0);
        viewer.in_flight_thumbs.insert("a.png".into());
        viewer.failed_thumbs.insert("b.png".into());
        viewer.in_flight.insert("c.png".into());
        assert_eq!(viewer.next_unthumbed(), Some(PathBuf::from("d.png")));
    }

    #[test]
    fn next_unthumbed_returns_none_when_exhausted() {
        let mut viewer = test_viewer(&["a.png"], 0);
        viewer.failed_thumbs.insert("a.png".into());
        assert_eq!(viewer.next_unthumbed(), None);
    }

    #[test]
    fn fresh_viewer_displays_nothing() {
        let viewer = test_viewer(&["a.png", "b.png"], 0);
        assert!(matches!(viewer.displayed, DisplayedImage::None));
        assert_eq!(viewer.displayed_path.as_deref(), None::<&Path>);
    }
}
