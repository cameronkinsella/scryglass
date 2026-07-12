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

impl From<crate::media::ThumbData> for Thumb {
    /// Wrap decoded thumbnail bytes in a drawable handle.
    fn from(data: crate::media::ThumbData) -> Self {
        Self {
            handle: Handle::from_rgba(data.width, data.height, data.pixels),
            size: (data.width, data.height),
            original_size: data.original_size,
        }
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
    /// An animation. Like a still, its on-screen pixels are derived at render time
    /// from the resource owner (here `anim_player`'s current frame texture, else
    /// the thumbnail blur), so the display owns nothing and never blanks: eviction
    /// frees the frames and the view falls back to the thumbnail.
    Animated { original_size: (u32, u32) },
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
    /// A released session a backgrounded window can re-open (decode decay). The
    /// `frame` stays on screen while this is set, so the video looks paused, not gone.
    pub suspended: Option<crate::video::SuspendedVideo>,
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
        // Clearing the suspended memo drops its archive temp guard, so navigating
        // away from a video that was released while backgrounded does not leak it.
        self.suspended = None;
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
    /// store owns. Holding it keeps the image resident at the demanded tier, and
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
    /// The width `filmstrip_scroll_x` was last computed against, so a resize
    /// can shift the strip by half the delta. Zero until the first resize
    /// stamps it. Kept through fullscreen (the strip is hidden there), so the
    /// round trip back lands where it started.
    pub filmstrip_width: f32,
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
    /// Resume point for the background thumbnail scan, so each completion
    /// continues where the walk left off instead of rescanning from the
    /// center (quadratic over the directory).
    thumb_scan: Option<ThumbScan>,
}

/// Frontier of one background thumbnail walk. Valid only for the focus it
/// was computed for: any center, range, or center-file change resets it.
struct ThumbScan {
    center: usize,
    range: std::ops::Range<usize>,
    /// The file at `center` when the walk started, so a resort under an
    /// unmoved cursor invalidates the frontier.
    center_path: PathBuf,
    /// Fan steps below this hold nothing pickable until the focus moves.
    next_d: usize,
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
            filmstrip_width: 0.0,
            visible_settle_pending: false,
            filmstrip_scrolled_at: Instant::now(),
            exif: None,
            rotation: 0,
            displayed_rotation: 0,
            video: VideoState::default(),
            resort_to_first: false,
            thumb_scan: None,
        }
    }

    /// Release the furthest ring of prefetched neighbors: among cached entries
    /// other than the on-screen image, find the greatest navigation distance
    /// and drop every lease at it, at most one per side. A path no longer in
    /// the listing counts as furthest of all. Returns true while prefetched
    /// entries remain cached after this ring, so the caller knows to come back.
    pub fn drop_prefetch_ring(&mut self) -> bool {
        let keep: HashSet<PathBuf> = std::iter::once(self.nav.current().to_path_buf())
            .chain(self.displayed_path.clone())
            .collect();
        let nav = &self.nav;
        // Each distance is a linear scan of the file list, so compute them
        // once instead of once for the max and again per retained entry.
        // Prefetched animations lease through the player, not the still
        // cache, and their frame sets are the heaviest entries of all, so
        // they shed in the same rings.
        let distances: Vec<(PathBuf, usize)> = self
            .cache
            .keys()
            .chain(self.anim_player.leased_paths())
            .filter(|p| !keep.contains(*p))
            .map(|p| (p.clone(), nav.distance(p).unwrap_or(usize::MAX)))
            .collect();
        let Some(ring) = distances.iter().map(|(_, d)| *d).max() else {
            return false;
        };
        for (path, distance) in &distances {
            if *distance == ring {
                self.cache.remove(path);
                self.anim_player.remove(path);
            }
        }
        self.cache.keys().any(|p| !keep.contains(p))
            || self.anim_player.leased_paths().any(|p| !keep.contains(p))
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
    /// produces a thumbnail anyway). Resumes from the walk's frontier, so a
    /// settled span is never rescanned and a thumb evicted behind the
    /// frontier is not re-decoded until the focus moves.
    pub fn next_unthumbed_in(
        &mut self,
        thumbs: &ImageCache<Thumb>,
        center: usize,
        range: std::ops::Range<usize>,
    ) -> Option<PathBuf> {
        let files = self.nav.files();
        let span = range.end.saturating_sub(range.start);
        let center_path = files.get(center).cloned().unwrap_or_default();
        let resume = match &self.thumb_scan {
            Some(scan)
                if scan.center == center
                    && scan.range == range
                    && scan.center_path == center_path =>
            {
                scan.next_d
            }
            _ => 0,
        };

        // Steps whose candidates are all permanently settled (thumbed, failed,
        // or never thumbable) advance the frontier. In-flight candidates only
        // pause it: a navigation can clear the in-flight set, and those slots
        // must be re-picked.
        let mut settled = resume;
        let mut found = None;
        'fan: for d in resume..span {
            let forward = center.checked_add(d).filter(|i| range.contains(i));
            let backward = (d > 0)
                .then(|| center.checked_sub(d))
                .flatten()
                .filter(|i| range.contains(i));
            let mut step_settled = true;
            for path in [forward, backward].into_iter().flatten().map(|i| &files[i]) {
                // Cheap set probes first; thumb_key allocates.
                if self.failed_thumbs.contains(path)
                    // Video thumbs need a real file (FFmpeg first-frame
                    // grab), so skip them inside archives.
                    || (crate::video::is_video(path) && !matches!(self.source, Source::Fs))
                {
                    continue;
                }
                if self.in_flight_thumbs.contains(path) {
                    step_settled = false;
                    continue;
                }
                if thumbs.contains(&thumb_key(&self.source, path)) {
                    continue;
                }
                found = Some(path.clone());
                break 'fan;
            }
            if step_settled && settled == d {
                settled = d + 1;
            }
        }
        self.thumb_scan = Some(ThumbScan {
            center,
            range,
            center_path,
            next_d: settled,
        });
        found
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
    fn next_unthumbed_skips_done_and_failed_but_not_in_flight_fulls() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png", "d.png"], 0);
        viewer.in_flight_thumbs.insert("a.png".into());
        viewer.failed_thumbs.insert("b.png".into());
        // A full load in flight still gets a thumb job: its persisted
        // thumbnail loads from disk long before the decode lands.
        viewer.in_flight.insert("c.png".into());
        assert_eq!(
            viewer.next_unthumbed_in(&ImageCache::new(0), 0, 0..4),
            Some(PathBuf::from("c.png"))
        );
    }

    #[test]
    fn next_unthumbed_returns_none_when_exhausted() {
        let mut viewer = test_viewer(&["a.png"], 0);
        viewer.failed_thumbs.insert("a.png".into());
        assert_eq!(viewer.next_unthumbed_in(&ImageCache::new(0), 0, 0..1), None);
    }

    fn test_thumb() -> Thumb {
        Thumb {
            handle: Handle::from_rgba(1, 1, vec![0u8; 4]),
            size: (1, 1),
            original_size: (1, 1),
        }
    }

    #[test]
    fn a_thumb_carries_its_data_dimensions() {
        let thumb = Thumb::from(crate::media::ThumbData {
            width: 2,
            height: 1,
            pixels: vec![0u8; 8],
            original_size: (200, 100),
        });
        assert_eq!(thumb.size, (2, 1));
        assert_eq!(thumb.original_size, (200, 100));
        assert_eq!(thumb.byte_cost(), 8);
    }

    #[test]
    fn a_thumb_evicted_behind_the_frontier_waits_for_a_focus_move() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png"], 1);
        let mut thumbs = ImageCache::new(usize::MAX);
        // Walk the whole range to completion: b, c, a.
        for name in ["b.png", "c.png", "a.png"] {
            assert_eq!(
                viewer.next_unthumbed_in(&thumbs, 1, 0..3),
                Some(PathBuf::from(name))
            );
            thumbs.insert(name.into(), test_thumb(), 1);
        }
        assert_eq!(viewer.next_unthumbed_in(&thumbs, 1, 0..3), None);

        // An eviction behind the frontier is not re-picked at the same focus,
        // so budget eviction cannot start a decode-evict loop.
        thumbs.remove(Path::new("b.png"));
        assert_eq!(viewer.next_unthumbed_in(&thumbs, 1, 0..3), None);

        // A focus move rescans and picks the hole up again.
        assert_eq!(
            viewer.next_unthumbed_in(&thumbs, 0, 0..3),
            Some(PathBuf::from("b.png"))
        );
    }

    #[test]
    fn a_resort_under_the_cursor_resets_the_scan_frontier() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png"], 0);
        let thumbs = ImageCache::new(usize::MAX);
        for name in ["a.png", "b.png", "c.png"] {
            viewer.failed_thumbs.insert(name.into());
        }
        assert_eq!(viewer.next_unthumbed_in(&thumbs, 0, 0..3), None);

        // A resort swaps in different files at the same indexes. The center
        // file changed, so the settled frontier must not carry over.
        viewer
            .nav
            .replace_files(vec!["x.png".into(), "y.png".into(), "z.png".into()]);
        assert_eq!(
            viewer.next_unthumbed_in(&thumbs, 0, 0..3),
            Some(PathBuf::from("x.png"))
        );
    }

    #[test]
    fn cleared_in_flight_slots_are_repicked_at_the_same_focus() {
        let mut viewer = test_viewer(&["a.png", "b.png"], 0);
        let thumbs = ImageCache::new(usize::MAX);
        assert_eq!(
            viewer.next_unthumbed_in(&thumbs, 0, 0..2),
            Some(PathBuf::from("a.png"))
        );
        viewer.in_flight_thumbs.insert("a.png".into());
        assert_eq!(
            viewer.next_unthumbed_in(&thumbs, 0, 0..2),
            Some(PathBuf::from("b.png"))
        );
        viewer.in_flight_thumbs.insert("b.png".into());
        assert_eq!(viewer.next_unthumbed_in(&thumbs, 0, 0..2), None);

        // A same-position navigation cancels and clears the in-flight set.
        // In-flight candidates only pause the frontier, so they come back.
        viewer.in_flight_thumbs.clear();
        assert_eq!(
            viewer.next_unthumbed_in(&thumbs, 0, 0..2),
            Some(PathBuf::from("a.png"))
        );
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
    /// The backing store is dropped right away. The lease keeps its shared cell
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
    fn prefetch_rings_drop_furthest_first_both_sides_at_once() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png", "d.png", "e.png"], 2);
        for name in ["a.png", "b.png", "c.png", "d.png", "e.png"] {
            cache_image(&mut viewer, name);
        }
        viewer.displayed_path = Some(PathBuf::from("c.png"));

        // First ring: the outermost pair (distance 2) goes, the near pair stays.
        assert!(viewer.drop_prefetch_ring());
        assert!(!viewer.cache.contains_key(Path::new("a.png")));
        assert!(!viewer.cache.contains_key(Path::new("e.png")));
        assert!(viewer.cache.contains_key(Path::new("b.png")));
        assert!(viewer.cache.contains_key(Path::new("d.png")));

        // Second ring: the near pair goes too. Nothing prefetched remains.
        assert!(!viewer.drop_prefetch_ring());
        assert!(viewer.cache.contains_key(Path::new("c.png")));
        assert_eq!(viewer.cache.len(), 1);

        // Nothing left to shed: still false, the on-screen image untouched.
        assert!(!viewer.drop_prefetch_ring());
        assert!(viewer.cache.contains_key(Path::new("c.png")));
    }

    #[test]
    fn prefetched_animations_shed_with_the_still_rings() {
        use crate::media::animation::AnimatedImage;
        use crate::media::store::{Anim, AnimRam, ImageKey, Store, Tier};
        use std::sync::Arc;

        // Five files so the wrap-aware distances differ: from c.png the
        // GIF sits at 2 and the still at 1.
        let mut viewer = test_viewer(&["a.gif", "b.png", "c.png", "d.png", "e.png"], 2);
        viewer.displayed_path = Some(PathBuf::from("c.png"));
        cache_image(&mut viewer, "b.png");

        // A prefetched neighbor GIF leases its decoded frames through the
        // player, not the still cache.
        let mut store: Store<Anim> = Store::default();
        let key = ImageKey::new(&Source::Fs, Path::new("a.gif"));
        let (lease, _) = store.request(key.clone(), "a.gif".into(), Source::Fs, Tier::InRam);
        store.on_decoded(
            key,
            AnimRam {
                frames: Arc::new(AnimatedImage {
                    width: 2,
                    height: 2,
                    frames: Vec::new(),
                    thumbnail: None,
                }),
                decode_time: None,
            },
        );
        viewer.anim_player.insert(PathBuf::from("a.gif"), lease);

        // The GIF's frames go in the first ring, and the shed reports the
        // nearer still remaining.
        assert!(viewer.drop_prefetch_ring());
        assert!(!viewer.anim_player.has_cached(Path::new("a.gif")));
        assert!(viewer.cache.contains_key(Path::new("b.png")));

        assert!(!viewer.drop_prefetch_ring());
        assert!(!viewer.cache.contains_key(Path::new("b.png")));
    }

    #[test]
    fn paths_outside_the_listing_shed_before_real_neighbors() {
        let mut viewer = test_viewer(&["a.png", "b.png", "c.png"], 1);
        cache_image(&mut viewer, "b.png");
        cache_image(&mut viewer, "c.png");
        cache_image(&mut viewer, "stale.png");
        viewer.displayed_path = Some(PathBuf::from("b.png"));

        // The stale path is not in the listing, so it counts as furthest.
        assert!(viewer.drop_prefetch_ring());
        assert!(!viewer.cache.contains_key(Path::new("stale.png")));
        assert!(viewer.cache.contains_key(Path::new("c.png")));
    }
}
