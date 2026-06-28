//! Session and viewer state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use iced::time::Instant;
use iced::widget::image::Handle;

use crate::anim::AnimPlayer;
use crate::media::cache::ImageCache;
use crate::media::pipeline::{Source, thumb_key};
use crate::media::store::Lease;
use crate::nav::Nav;

/// Thumbnail cache budget, about 500 thumbs. Split across live windows.
pub(crate) const THUMB_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Whether the app is idle or actively viewing a directory of images.
pub enum Session {
    /// Waiting for a file drop or open.
    Empty,
    /// Actively viewing images.
    Viewing(Box<Viewer>),
}

/// A small preview texture, used by the filmstrip and as the blurred
/// placeholder while the full image decodes.
#[derive(Debug, Clone)]
pub struct Thumb {
    pub handle: Handle,
    /// Thumbnail texture dimensions.
    pub size: (u32, u32),
    /// True dimensions of the previewed image, for placeholder zoom geometry.
    pub original_size: (u32, u32),
}

impl Thumb {
    /// Approximate memory cost in bytes (RGBA8).
    pub fn byte_cost(&self) -> usize {
        self.size.0 as usize * self.size.1 as usize * 4
    }
}

/// What the image area is currently showing.
#[derive(Default)]
pub enum DisplayedImage {
    /// Nothing yet, first image still loading.
    #[default]
    None,
    /// A blurred low-res stand-in while the full image decodes.
    Placeholder(Thumb),
    /// A decoded still, identified by `displayed_path`. The on-screen pixels are
    /// derived entirely from the shared store at render time: the texture from the
    /// current cache entry's lease cell (or `rotated`, a view-local rotation
    /// override), else the thumbnail blur. The display owns no texture and no RAM,
    /// so it can never diverge from what the store holds and never pins memory: a
    /// background window's decay can free its RAM while it keeps showing the image
    /// through another window's shared texture. A black frame is unrepresentable.
    Full {
        original_size: (u32, u32),
        rotated: Option<crate::ui::image_surface::Keepalive>,
    },
    /// An animation frame. Like a still it draws a resident `texture` directly;
    /// `handle` is kept only for clipboard copy. Animations are not store-backed
    /// (their frames are transient, re-uploaded each tick), so the texture is
    /// carried here rather than read from a store lease cell.
    Animated {
        handle: Handle,
        texture: crate::ui::image_surface::Keepalive,
        original_size: (u32, u32),
    },
    /// Live video, drawn by the GPU YUV surface. Carries dimensions for
    /// zoom and the info panel. The frame planes live on the viewer.
    Video { original_size: (u32, u32) },
    /// The file couldn't be decoded, so the image area shows this message
    /// instead of an image.
    Error { message: String },
}

impl DisplayedImage {
    /// True image dimensions, if anything is displayed.
    pub fn original_size(&self) -> Option<(u32, u32)> {
        match self {
            DisplayedImage::None => None,
            DisplayedImage::Placeholder(thumb) => Some(thumb.original_size),
            DisplayedImage::Full { original_size, .. } => Some(*original_size),
            DisplayedImage::Animated { original_size, .. } => Some(*original_size),
            DisplayedImage::Video { original_size } => Some(*original_size),
            DisplayedImage::Error { .. } => None,
        }
    }
}

/// All video playback state for a viewer: the decode session, the latest frame,
/// the seek drag, archive extraction, and the controls overlay. Grouped so
/// navigation tears the active video down in one place.
#[derive(Default)]
pub struct VideoState {
    /// Active playback session (feature `video`). Dropping it stops the decode
    /// threads.
    pub session: Option<crate::video::VideoSession>,
    /// Latest decoded frame, drawn by the GPU YUV surface.
    pub frame: Option<std::sync::Arc<crate::video::VideoFrame>>,
    /// Mid-drag value of the seek slider (seconds), committed on release.
    pub seek_drag: Option<f64>,
    /// Archive video entry currently extracting to a temp file.
    pub extracting: Option<PathBuf>,
    /// Controls stay visible until this deadline, refreshed by mouse movement.
    pub controls_until: Option<Instant>,
    /// Control-bar fade level, eased toward 0 or 1 each tick.
    pub controls_opacity: f32,
}

impl VideoState {
    /// Tear down the active video (session, frame, seek, extraction), as
    /// navigating away from a video does. The controls overlay is left to ease
    /// out on its own, since it only renders while a session exists.
    pub fn reset(&mut self) {
        self.session = None;
        self.frame = None;
        self.seek_drag = None;
        self.extracting = None;
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
    /// This window's store leases, keyed by path: the on-screen image plus its
    /// prefetch neighbors. Each lease is a claim on the one shared texture the
    /// store owns; holding it keeps the image resident at the demanded tier, and
    /// dropping it (navigation, decay, close) lowers the demand. The set is the
    /// window's whole still-image footprint: no separate warm pool.
    pub cache: HashMap<PathBuf, Lease>,
    /// Paths with a full load currently in flight, to avoid duplicate decodes.
    pub in_flight: HashSet<PathBuf>,
    /// Paths with a thumbnail probe in flight.
    pub in_flight_thumbs: HashSet<PathBuf>,
    /// Paths whose background thumbnail attempt failed (corrupt or
    /// undecodable), never re-picked by the thumbnailer.
    pub failed_thumbs: HashSet<PathBuf>,
    /// Paths whose full decode failed, mapped to the error to show. Keeps a
    /// broken file navigable (it displays the message) instead of a dead end.
    pub failed_loads: HashMap<PathBuf, String>,
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
    /// Whether a dwell-load check is already scheduled, so a moving slider
    /// reuses one timer instead of spawning one per step.
    pub dwell_pending: bool,
    /// Which direction key is currently held, and when the hold started.
    pub held_direction: Option<(Direction, Instant)>,
    /// Which edge strip the cursor is over.
    pub edge_hover: Option<Direction>,
    /// An edge strip held with the mouse, pacing the repeat timer.
    pub edge_held: Option<Direction>,
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
    /// Whether a filmstrip-settle check is scheduled, so a moving scroll
    /// reuses one timer instead of arming one per scroll event.
    pub visible_settle_pending: bool,
    /// When the filmstrip scroll offset last changed, so the settle check can
    /// tell scrolling has stopped.
    pub filmstrip_scrolled_at: Instant,
    /// EXIF fields for the info panel, tagged with the file they describe.
    pub exif: Option<(PathBuf, Vec<(String, String)>)>,
    /// Desired view rotation in quarter turns clockwise (0-3).
    /// Non-destructive, reset when navigating to another image.
    pub rotation: u8,
    /// Rotation currently baked into the displayed texture. When this
    /// trails `rotation`, a rotate task is producing the next texture.
    pub displayed_rotation: u8,
    /// All video playback state: the decode session, latest frame, seek drag,
    /// archive extraction, and the controls overlay.
    pub video: VideoState,
    /// After the post-open resort lands, jump to the first image of the
    /// new order. Set when a folder or archive was opened rather than a
    /// specific file.
    pub resort_to_first: bool,
}

impl Viewer {
    /// Fresh viewer for a newly scanned directory or archive, with the
    /// first load and metadata probe pending.
    pub fn new(nav: Nav, source: Source, anim_player: AnimPlayer) -> Self {
        Self {
            nav,
            source,
            displayed: DisplayedImage::None,
            // Memory is the leased set: this map holds the on-screen image plus
            // its prefetch neighbors, pruned by `retain`, with no byte budget.
            cache: HashMap::new(),
            in_flight: HashSet::new(),
            in_flight_thumbs: HashSet::new(),
            failed_thumbs: HashSet::new(),
            failed_loads: HashMap::new(),
            displayed_path: None,
            pending_since: Some(Instant::now()),
            pending_nav: None,
            slider_drag: None,
            dwell_pending: false,
            held_direction: None,
            edge_hover: None,
            edge_held: None,
            anim_player,
            current_file_size: None,
            zoom: 1.0,
            manual_zoom: false,
            pan: (0.0, 0.0),
            drag: None,
            filmstrip_scroll_x: 0.0,
            visible_settle_pending: false,
            filmstrip_scrolled_at: Instant::now(),
            exif: None,
            rotation: 0,
            displayed_rotation: 0,
            video: VideoState::default(),
            resort_to_first: false,
        }
    }

    /// Shed the prefetch look-ahead down to just the on-screen image, freeing
    /// the neighbors' VRAM and RAM. Called when a window has sat unfocused long
    /// enough that its look-ahead is unlikely to be used soon; refocus re-warms
    /// it. Thumbnails are kept (small, and the filmstrip draws them).
    pub fn drop_prefetch(&mut self) {
        let keep: HashSet<PathBuf> = std::iter::once(self.nav.current().to_path_buf())
            .chain(self.displayed_path.clone())
            .collect();
        self.cache.retain(|p, _| keep.contains(p));
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

    /// Whether `path` has a resident shared texture this window can draw right
    /// now (its lease's cell is filled). The store-era replacement for the old
    /// "is it cached" check: a lease without a texture (still decoding, or
    /// decayed) does not count, so navigation never moves onto a blank.
    fn has_resident_texture(&self, path: &std::path::Path) -> bool {
        self.cache
            .get(path)
            .is_some_and(|lease| lease.texture().is_some())
    }

    /// Whether anything can be put on screen for `path` right now:
    /// a resident image, a thumbnail (blur), or a cached GIF.
    pub fn displayable(&self, thumbs: &ImageCache<Thumb>, path: &std::path::Path) -> bool {
        self.has_resident_texture(path)
            || thumbs.contains(&thumb_key(&self.source, path))
            || self.anim_player.has_cached(path)
            // Videos display as soon as their first frame decodes, so
            // navigation never waits on them.
            || crate::video::is_video(path)
            // A known-bad file shows its error in place, so the cursor can
            // land on it rather than stalling before it.
            || self.failed_loads.contains_key(path)
    }

    /// Whether the sharp image for `path` is already in hand, so a resting
    /// slider has nothing left to load. A thumbnail blur does not count.
    pub fn has_full(&self, path: &std::path::Path) -> bool {
        self.has_resident_texture(path)
            || self.anim_player.has_cached(path)
            || crate::video::is_video(path)
            || self.failed_loads.contains_key(path)
    }

    /// The next file for the background thumbnailer to pick within `range`,
    /// fanning outward from `center` (center, +1, -1, +2, -2, ...): one with
    /// no thumbnail, none in flight, and no full load underway (a full load
    /// produces a thumbnail anyway).
    pub fn next_unthumbed_in(
        &self,
        thumbs: &ImageCache<Thumb>,
        center: usize,
        range: std::ops::Range<usize>,
    ) -> Option<PathBuf> {
        let files = self.nav.files();
        let span = range.end.saturating_sub(range.start);
        (0..span)
            .flat_map(|d| {
                let forward = center.checked_add(d).filter(|i| range.contains(i));
                let backward = (d > 0)
                    .then(|| center.checked_sub(d))
                    .flatten()
                    .filter(|i| range.contains(i));
                [forward, backward]
            })
            .flatten()
            .map(|i| &files[i])
            .find(|p| {
                !thumbs.contains(&thumb_key(&self.source, p))
                    && !self.in_flight_thumbs.contains(*p)
                    && !self.failed_thumbs.contains(*p)
                    && !self.in_flight.contains(*p)
                    // Video thumbs need a real file (FFmpeg first-frame
                    // grab), so skip them inside archives.
                    && (!crate::video::is_video(p) || matches!(self.source, Source::Fs))
            })
            .cloned()
    }

    /// The on-disk file behind the current image: the file itself, or the
    /// archive containing it.
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
    /// When `target` was last set, so a slider resting here can load it.
    pub since: Instant,
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
        Viewer::new(nav, Source::Fs, AnimPlayer::new())
    }

    #[test]
    fn next_unthumbed_fans_outward_from_the_cursor() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png", "d.png", "e.png"], 2);
        // Cursor first, then alternate outward: c, d, b, e, a.
        for expected in ["c.png", "d.png", "b.png", "e.png", "a.png"] {
            assert_eq!(
                viewer.next_unthumbed_in(&ImageCache::new(0), 2, 0..5),
                Some(PathBuf::from(expected))
            );
            viewer.in_flight_thumbs.insert(PathBuf::from(expected));
        }
        assert_eq!(viewer.next_unthumbed_in(&ImageCache::new(0), 2, 0..5), None);
    }

    #[test]
    fn next_unthumbed_in_fans_outward_within_a_subrange() {
        let mut viewer = test_viewer(&["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"], 0);
        // Window 3..8 centered on 5: 5, 6, 4, 7, 3 and never 2 or 8.
        for expected in ["5", "6", "4", "7", "3"] {
            assert_eq!(
                viewer.next_unthumbed_in(&ImageCache::new(0), 5, 3..8),
                Some(PathBuf::from(expected))
            );
            viewer.in_flight_thumbs.insert(PathBuf::from(expected));
        }
        assert_eq!(viewer.next_unthumbed_in(&ImageCache::new(0), 5, 3..8), None);
    }

    #[test]
    fn next_unthumbed_in_never_leaves_the_range() {
        let mut viewer = test_viewer(&["0", "1", "2", "3", "4"], 0);
        // Window 1..4 centered on 2: 2, 3, 1 and never the out-of-range 0 or 4.
        for expected in ["2", "3", "1"] {
            assert_eq!(
                viewer.next_unthumbed_in(&ImageCache::new(0), 2, 1..4),
                Some(PathBuf::from(expected))
            );
            viewer.in_flight_thumbs.insert(PathBuf::from(expected));
        }
        assert_eq!(viewer.next_unthumbed_in(&ImageCache::new(0), 2, 1..4), None);
    }

    #[test]
    fn next_unthumbed_skips_done_in_flight_and_failed() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png", "d.png"], 0);
        viewer.in_flight_thumbs.insert("a.png".into());
        viewer.failed_thumbs.insert("b.png".into());
        viewer.in_flight.insert("c.png".into());
        assert_eq!(
            viewer.next_unthumbed_in(&ImageCache::new(0), 0, 0..4),
            Some(PathBuf::from("d.png"))
        );
    }

    #[test]
    fn next_unthumbed_returns_none_when_exhausted() {
        let mut viewer = test_viewer(&["a.png"], 0);
        viewer.failed_thumbs.insert("a.png".into());
        assert_eq!(viewer.next_unthumbed_in(&ImageCache::new(0), 0, 0..1), None);
    }

    #[test]
    fn fresh_viewer_displays_nothing() {
        let viewer = test_viewer(&["a.png", "b.png"], 0);
        assert!(matches!(viewer.displayed, DisplayedImage::None));
        assert_eq!(viewer.displayed_path.as_deref(), None::<&Path>);
    }

    #[test]
    fn a_failed_load_is_displayable_so_the_cursor_can_land() {
        let mut viewer = test_viewer(&["a.png", "b.png"], 0);
        assert!(!viewer.displayable(&ImageCache::new(0), Path::new("b.png")));
        viewer
            .failed_loads
            .insert("b.png".into(), "could not decode".into());
        assert!(viewer.displayable(&ImageCache::new(0), Path::new("b.png")));
    }

    /// A lease holding a resident full-res texture, as after a decode and upload.
    /// The backing store is dropped right away; the lease keeps its shared cell
    /// alive on its own (the cell is `Arc`), so `lease.texture()` stays `Some` and
    /// the image counts as displayable.
    fn resident_lease(path: &str) -> Lease {
        use crate::media::store::{ImageKey, RamImage, Store, Tier};
        let mut store = Store::default();
        let p = PathBuf::from(path);
        let key = ImageKey::new(&Source::Fs, &p);
        let (lease, _) = store.request(key.clone(), p, Source::Fs, Tier::Full);
        store.on_decoded(
            key.clone(),
            RamImage {
                handle: Handle::from_rgba(2, 2, vec![0u8; 16]),
                original_size: (2, 2),
                decode_time: None,
            },
        );
        store.on_minted(key, Tier::Full, crate::ui::image_surface::test_keepalive());
        lease
    }

    fn cache_image(viewer: &mut Viewer, path: &str) {
        viewer
            .cache
            .insert(PathBuf::from(path), resident_lease(path));
    }

    #[test]
    fn a_resident_cache_entry_is_displayable() {
        let mut viewer = test_viewer(&["a.png", "b.png"], 0);
        // Nothing leased yet: not displayable as a sharp image.
        assert!(!viewer.displayable(&ImageCache::new(0), Path::new("a.png")));
        cache_image(&mut viewer, "a.png");
        // The lease holds a resident texture, so the image can go on screen.
        assert!(viewer.displayable(&ImageCache::new(0), Path::new("a.png")));
        assert!(viewer.has_full(Path::new("a.png")));
    }

    #[test]
    fn drop_prefetch_keeps_only_the_on_screen_image() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png"], 1);
        cache_image(&mut viewer, "a.png");
        cache_image(&mut viewer, "b.png");
        cache_image(&mut viewer, "c.png");
        viewer.displayed_path = Some(PathBuf::from("b.png"));

        viewer.drop_prefetch();

        assert!(viewer.cache.contains_key(Path::new("b.png")));
        assert!(!viewer.cache.contains_key(Path::new("a.png")));
        assert!(!viewer.cache.contains_key(Path::new("c.png")));
    }
}
