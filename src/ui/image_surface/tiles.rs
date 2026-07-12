//! The resident tile pyramid for stills too large for one texture: bounded
//! tile caches, production claims, and the draw stamps the demand pass reads
//! back. The grid math itself lives in `crate::media::tiles`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::media::tiles::{TileCache, TileKey};

use super::resident::Keepalive;
use super::uniforms::UNIFORM_SLOTS;

/// Most tiles a pyramid keeps resident: a 4K viewport's worst-case wanted set
/// (see [`UNIFORM_SLOTS`]) plus headroom, so a demand wave never evicts its own
/// freshly produced tiles. Older tiles drop (freeing their VRAM) and are
/// re-produced from the RAM source on return.
const MAX_CACHED_TILES: usize = 224;

/// Most exact-scale tiles kept resident per target: the visible set plus pan
/// margin (a 4K viewport at one texel per pixel shows ceil(3840/512+1) x
/// ceil(2160/512+1) = 9x6 = 54).
const MAX_EXACT_TILES: usize = 96;

/// Most exact-scale targets kept at once: the pyramid is shared cross-window,
/// so two windows resting on the same image at their own sizes each keep a
/// layer, plus one recent size so a resize bounce finds its tiles instead of
/// re-cutting them. Each layer holds at most a viewport's worth of view-size
/// tiles, so the cap costs a few screen-size copies of VRAM. The least
/// recently drawn or demanded target evicts first.
const MAX_EXACT_TARGETS: usize = 3;

/// One exact-scale layer: tiles that are byte-exact crops of the one-pass
/// downscale at the layer's target size, keyed by grid position with `lod`
/// fixed at 0.
struct ExactLayer {
    tiles: TileCache<Keepalive>,
    pending: std::collections::HashMap<TileKey, std::time::Instant>,
}

impl ExactLayer {
    fn new() -> Self {
        Self {
            tiles: TileCache::new(MAX_EXACT_TILES),
            pending: std::collections::HashMap::new(),
        }
    }
}

/// One tiled draw's record: what `prepare_tiles` placed and selected. The
/// demand pass reads these back so production always targets the size and
/// level a real placement sampled, never a recomputation that could disagree
/// by a rounding step.
#[derive(Clone, Copy)]
pub struct DrawStamp {
    /// The physical size the whole image spanned: the exact layer's target.
    pub shown: (u32, u32),
    /// The window scale factor the draw ran at.
    pub scale: f32,
    /// What the draw selected: the base alone or a mip level.
    pub want: DrawWant,
}

/// The resident form of a tiled still: its bounded tile cache plus the
/// uncapped source size the tile grid maps. Shared cross-window inside one
/// [`Keepalive`], with tiles streaming in through the mutex as they land.
pub struct TileSet {
    original: (u32, u32),
    /// The view-quality layer beneath the tiles, re-derived at resting zooms
    /// so it stays a one-pass copy (a fixed base redrawn through the kernel
    /// would be softer than the View tier it must match).
    base: Mutex<Keepalive>,
    /// Exact-scale tile layers keyed by target size, most recently used
    /// first: byte-exact crops of the one-pass whole-image downscale,
    /// produced for the visible region only, so a rest costs viewport work
    /// instead of whole-image work. One layer per window resting on the
    /// image at its own size, so windows sharing the pyramid never wipe
    /// each other's tiles. Bounded by [`MAX_EXACT_TARGETS`].
    exact: Mutex<Vec<((u32, u32), ExactLayer)>>,
    /// The latest draw stamp per window, keyed by window id. Each window
    /// stamps its own placement and the demand pass reads back its own
    /// window's last draw directly, so concurrent windows never adopt each
    /// other's target and a resize follows this window's live size.
    stamps: Mutex<std::collections::HashMap<iced::window::Id, DrawStamp>>,
    tiles: Mutex<TileCache<Keepalive>>,
    /// Tiles requested but not yet landed, with their claim time, so a pan or
    /// zoom repeating its demand pass never produces the same tile twice. A
    /// claim whose settle message was lost (its window closed mid-production)
    /// expires rather than blocking the tile for the pyramid's lifetime.
    pending: Mutex<std::collections::HashMap<TileKey, std::time::Instant>>,
    /// The mip level each window's latest demand pass asked for, unioned
    /// into `wanted_mask`. A queued production for a level no window wants
    /// bails before its resample: a zoom that keeps moving obsoletes whole
    /// waves of tiles. Kept per window so a zoom cancels only that window's
    /// own stale wave, never a sibling resting at another level on the same
    /// shared pyramid. Bounded by the pyramid's lifetime, like `stamps`.
    wanted_lods: Mutex<std::collections::HashMap<iced::window::Id, u32>>,
    /// Bit `l` set when some window wants level `l` (levels stay far below 64).
    wanted_mask: AtomicU64,
}

/// Most tiles one demand pass may claim: what one frame can draw.
pub const MAX_TILE_DRAWS: usize = UNIFORM_SLOTS as usize - 1;

/// How long a tile claim blocks re-requests before it is presumed lost. Far
/// past any real produce plus upload, so it only fires for a settle message
/// that will never arrive.
const CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// What a tiled draw selected, recorded in its stamp for the demand pass.
#[derive(Clone, Copy)]
pub enum DrawWant {
    /// The base layer sufficed (exact tiles draw over it when resident).
    BaseOnly,
    /// The draw sampled this mip level.
    Level(u32),
}

impl TileSet {
    /// A fresh, empty pyramid for an `original`-sized still with `base` as
    /// its view-quality layer.
    pub(super) fn new(original: (u32, u32), base: Keepalive) -> Self {
        Self {
            original,
            base: Mutex::new(base),
            exact: Mutex::new(Vec::new()),
            stamps: Mutex::new(std::collections::HashMap::new()),
            tiles: Mutex::new(TileCache::new(MAX_CACHED_TILES)),
            pending: Mutex::new(std::collections::HashMap::new()),
            wanted_lods: Mutex::new(std::collections::HashMap::new()),
            wanted_mask: AtomicU64::new(0),
        }
    }

    /// The uncapped source dimensions the pyramid maps.
    pub fn original(&self) -> (u32, u32) {
        self.original
    }

    /// The view-quality layer beneath the tiles.
    pub fn base(&self) -> Option<Keepalive> {
        self.base.lock().ok().map(|base| base.clone())
    }

    /// Ready an exact layer for `target`, creating it when absent and marking
    /// it most recently used. Past [`MAX_EXACT_TARGETS`] the stalest target
    /// drops (its VRAM frees off-thread as the keepalives drop). Other
    /// targets keep their tiles and claims, so windows sharing the pyramid
    /// at different sizes coexist.
    pub fn ensure_exact(&self, target: (u32, u32)) {
        let Ok(mut layers) = self.exact.lock() else {
            return;
        };
        if let Some(pos) = layers.iter().position(|(t, _)| *t == target) {
            let layer = layers.remove(pos);
            layers.insert(0, layer);
            return;
        }
        layers.insert(0, (target, ExactLayer::new()));
        layers.truncate(MAX_EXACT_TARGETS);
    }

    /// Whether an exact layer for `target` is still resident. Productions in
    /// flight for an evicted target bail on this.
    pub fn exact_serves(&self, target: (u32, u32)) -> bool {
        self.exact
            .lock()
            .map(|layers| layers.iter().any(|(t, _)| *t == target))
            .unwrap_or(false)
    }

    /// Whether an exact layer serves `target`, marking it drawn so eviction
    /// at the target cap spares the layers windows still render.
    pub(super) fn exact_drawn(&self, target: (u32, u32)) -> bool {
        let Ok(mut layers) = self.exact.lock() else {
            return false;
        };
        let Some(pos) = layers.iter().position(|(t, _)| *t == target) else {
            return false;
        };
        let layer = layers.remove(pos);
        layers.insert(0, layer);
        true
    }

    /// Claim one exact tile for production: false when it is resident, in
    /// flight (unexpired), or its target's layer was evicted.
    pub fn try_claim_exact(&self, target: (u32, u32), key: TileKey) -> bool {
        let Ok(mut layers) = self.exact.lock() else {
            return false;
        };
        let Some((_, layer)) = layers.iter_mut().find(|(t, _)| *t == target) else {
            return false;
        };
        if layer.tiles.contains(key) {
            return false;
        }
        match layer.pending.get(&key) {
            Some(claimed) if claimed.elapsed() < CLAIM_TTL => false,
            _ => {
                layer.pending.insert(key, std::time::Instant::now());
                true
            }
        }
    }

    /// A production for `target` finished: release its claim and install the
    /// texture, unless that target's layer was evicted in the meantime.
    /// Other targets are never touched.
    pub fn settle_exact(&self, target: (u32, u32), key: TileKey, texture: Option<Keepalive>) {
        if let Ok(mut layers) = self.exact.lock()
            && let Some((_, layer)) = layers.iter_mut().find(|(t, _)| *t == target)
        {
            layer.pending.remove(&key);
            if let Some(texture) = texture {
                layer.tiles.insert(key, texture);
            }
        }
    }

    /// A resident exact tile for `target`, refreshing its recency within its
    /// layer.
    pub(super) fn exact_get(&self, target: (u32, u32), key: TileKey) -> Option<Keepalive> {
        self.exact.lock().ok().and_then(|mut layers| {
            layers
                .iter_mut()
                .find(|(t, _)| *t == target)
                .and_then(|(_, layer)| layer.tiles.get(key).cloned())
        })
    }

    /// Record what a tiled draw placed and selected, replacing this window's
    /// previous stamp. Keyed by window id, so a window always reads back its
    /// own latest draw.
    pub(super) fn stamp_draw(&self, window: iced::window::Id, stamp: DrawStamp) {
        let Ok(mut stamps) = self.stamps.lock() else {
            return;
        };
        stamps.insert(window, stamp);
    }

    /// This window's last tiled draw. Read directly, no size matching, so
    /// demand tracks the live size even mid-resize when the estimate drifts
    /// from the drawn size. `None` before this window's first tiled draw.
    pub fn draw_stamp_for(&self, window: iced::window::Id) -> Option<DrawStamp> {
        self.stamps.lock().ok()?.get(&window).copied()
    }

    /// Production started: restart the tile's claim clock, so the TTL
    /// measures the work, not the queue behind the gate.
    pub fn refresh_claim(&self, key: TileKey) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(claimed) = pending.get_mut(&key)
        {
            *claimed = std::time::Instant::now();
        }
    }

    /// Install a produced tile, evicting the stalest past the cap.
    pub fn insert(&self, key: TileKey, tile: Keepalive) {
        if let Ok(mut tiles) = self.tiles.lock() {
            tiles.insert(key, tile);
        }
    }

    /// The tile for `key`, freshly marked as used.
    pub fn get(&self, key: TileKey) -> Option<Keepalive> {
        self.tiles.lock().ok()?.get(key).cloned()
    }

    /// Claim `key` for production: true when it is neither resident nor
    /// already in flight, marking it in flight. An expired claim (its settle
    /// message was lost) counts as absent.
    pub fn try_claim(&self, key: TileKey) -> bool {
        let resident = self
            .tiles
            .lock()
            .map(|tiles| tiles.contains(key))
            .unwrap_or(true);
        if resident {
            return false;
        }
        self.pending
            .lock()
            .map(|mut pending| match pending.get(&key) {
                Some(claimed) if claimed.elapsed() < CLAIM_TTL => false,
                _ => {
                    pending.insert(key, std::time::Instant::now());
                    true
                }
            })
            .unwrap_or(false)
    }

    /// A production for `key` finished (either way). Its claim is released.
    pub fn settle(&self, key: TileKey) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&key);
        }
    }

    /// Record the level `window`'s current view wants. Its own stale
    /// productions bail; other windows' levels stay wanted.
    pub fn set_wanted_lod(&self, window: iced::window::Id, lod: u32) {
        if let Ok(mut wants) = self.wanted_lods.lock() {
            wants.insert(window, lod);
            let mask = wants
                .values()
                .fold(0u64, |mask, &l| mask | 1u64 << l.min(63));
            self.wanted_mask.store(mask, Ordering::Relaxed);
        }
    }

    /// Whether any window's latest demand pass wants `lod`.
    pub fn wants_lod(&self, lod: u32) -> bool {
        self.wanted_mask.load(Ordering::Relaxed) & (1u64 << lod.min(63)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::super::resident::test_keepalive;
    use super::*;

    fn key(col: u32, row: u32) -> TileKey {
        TileKey { lod: 0, col, row }
    }

    fn set() -> TileSet {
        TileSet::new((8192, 8192), test_keepalive())
    }

    fn stamp(shown: (u32, u32), scale: f32, want: DrawWant) -> DrawStamp {
        DrawStamp { shown, scale, want }
    }

    #[test]
    fn a_claim_older_than_the_ttl_can_be_reclaimed() {
        let set = set();
        assert!(set.try_claim(key(0, 0)));
        assert!(!set.try_claim(key(0, 0)), "a fresh claim blocks re-claims");
        // Backdate the claim past the TTL, as if its settle message was lost.
        let expired = std::time::Instant::now()
            .checked_sub(CLAIM_TTL + std::time::Duration::from_secs(1))
            .expect("clock reaches past the TTL");
        set.pending.lock().unwrap().insert(key(0, 0), expired);
        assert!(set.try_claim(key(0, 0)));
    }

    #[test]
    fn an_exact_claim_older_than_the_ttl_can_be_reclaimed() {
        let set = set();
        set.ensure_exact((800, 600));
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
        assert!(!set.try_claim_exact((800, 600), key(0, 0)));
        let expired = std::time::Instant::now()
            .checked_sub(CLAIM_TTL + std::time::Duration::from_secs(1))
            .expect("clock reaches past the TTL");
        set.exact.lock().unwrap()[0]
            .1
            .pending
            .insert(key(0, 0), expired);
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
    }

    #[test]
    fn a_resident_tile_is_never_claimed() {
        let set = set();
        set.insert(key(0, 0), test_keepalive());
        assert!(!set.try_claim(key(0, 0)));
    }

    #[test]
    fn two_exact_targets_coexist() {
        let set = set();
        set.ensure_exact((800, 600));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        set.ensure_exact((400, 300));
        set.settle_exact((400, 300), key(0, 0), Some(test_keepalive()));
        // Readying the second target left the first target's tiles alone.
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
        assert!(set.exact_get((400, 300), key(0, 0)).is_some());
        assert!(set.exact_serves((800, 600)));
        assert!(set.exact_serves((400, 300)));
    }

    #[test]
    fn eviction_at_the_cap_drops_the_least_recently_drawn_target() {
        let set = set();
        set.ensure_exact((800, 600));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        // Fill the remaining slots with other targets.
        for i in 0..(MAX_EXACT_TARGETS as u32 - 1) {
            set.ensure_exact((100 + i, 100 + i));
        }
        // A draw of (800, 600) refreshes it, so the first filler is stalest.
        assert!(set.exact_drawn((800, 600)));
        set.ensure_exact((999, 999));
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
        assert!(!set.exact_serves((100, 100)));
        assert!(set.exact_serves((999, 999)));
    }

    #[test]
    fn settle_for_one_target_does_not_disturb_another() {
        let set = set();
        set.ensure_exact((800, 600));
        set.ensure_exact((400, 300));
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
        assert!(set.try_claim_exact((400, 300), key(0, 0)));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
        // The other target still holds its claim and has no tile.
        assert!(set.exact_get((400, 300), key(0, 0)).is_none());
        assert!(
            !set.try_claim_exact((400, 300), key(0, 0)),
            "claim survives"
        );
        set.settle_exact((400, 300), key(0, 0), Some(test_keepalive()));
        assert!(set.exact_get((400, 300), key(0, 0)).is_some());
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
    }

    #[test]
    fn settle_exact_after_an_eviction_is_a_no_op() {
        let set = set();
        set.ensure_exact((800, 600));
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
        // Push the target out of the cap.
        for i in 0..MAX_EXACT_TARGETS as u32 {
            set.ensure_exact((100 + i, 100 + i));
        }
        assert!(!set.exact_serves((800, 600)));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        // Coming back finds nothing landed and no claim held.
        set.ensure_exact((800, 600));
        assert!(set.exact_get((800, 600), key(0, 0)).is_none());
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
    }

    #[test]
    fn ensure_exact_at_the_same_target_keeps_tiles_and_claims() {
        let set = set();
        set.ensure_exact((800, 600));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        assert!(set.try_claim_exact((800, 600), key(1, 0)));
        set.ensure_exact((800, 600));
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
        assert!(
            !set.try_claim_exact((800, 600), key(1, 0)),
            "claim survives"
        );
    }

    #[test]
    fn exact_drawn_only_finds_ensured_targets() {
        let set = set();
        assert!(!set.exact_drawn((800, 600)));
        set.ensure_exact((800, 600));
        assert!(set.exact_drawn((800, 600)));
        assert!(!set.exact_drawn((400, 300)));
    }

    #[test]
    fn a_window_reads_back_its_own_stamp() {
        let set = set();
        let win = iced::window::Id::unique();
        assert!(set.draw_stamp_for(win).is_none());
        set.stamp_draw(win, stamp((1000, 800), 1.0, DrawWant::BaseOnly));
        let found = set.draw_stamp_for(win).expect("stamped");
        assert_eq!(found.shown, (1000, 800));
        assert_eq!(found.scale, 1.0);
        assert!(matches!(found.want, DrawWant::BaseOnly));
    }

    #[test]
    fn a_second_windows_stamp_does_not_change_the_first() {
        let set = set();
        let a = iced::window::Id::unique();
        let b = iced::window::Id::unique();
        set.stamp_draw(a, stamp((1000, 800), 1.0, DrawWant::BaseOnly));
        set.stamp_draw(b, stamp((1990, 1590), 2.0, DrawWant::Level(1)));
        // Each window reads its own draw, never the other's.
        let found = set.draw_stamp_for(a).expect("stamped");
        assert_eq!(found.shown, (1000, 800));
        assert!(matches!(found.want, DrawWant::BaseOnly));
        let found = set.draw_stamp_for(b).expect("stamped");
        assert_eq!(found.shown, (1990, 1590));
        assert!(matches!(found.want, DrawWant::Level(1)));
    }

    #[test]
    fn restamping_a_window_replaces_its_entry() {
        let set = set();
        let win = iced::window::Id::unique();
        set.stamp_draw(win, stamp((1000, 800), 1.0, DrawWant::Level(0)));
        set.stamp_draw(win, stamp((1000, 800), 1.0, DrawWant::BaseOnly));
        assert_eq!(set.stamps.lock().unwrap().len(), 1);
        let found = set.draw_stamp_for(win).expect("stamped");
        assert!(matches!(found.want, DrawWant::BaseOnly));
    }

    #[test]
    fn a_resize_reads_the_live_size_never_a_stale_one() {
        // The regression: a shrink restamps a window at a new size, and the
        // demand pass must read that size, not the size before the shrink.
        let set = set();
        let a = iced::window::Id::unique();
        let b = iced::window::Id::unique();
        set.stamp_draw(a, stamp((2000, 1600), 1.0, DrawWant::BaseOnly));
        set.stamp_draw(a, stamp((1000, 800), 1.0, DrawWant::BaseOnly));
        // A reads its current draw (S2), never the pre-shrink S1.
        let found = set.draw_stamp_for(a).expect("stamped");
        assert_eq!(found.shown, (1000, 800));
        // A concurrent window stamping a different size leaves A on S2.
        set.stamp_draw(b, stamp((640, 480), 1.0, DrawWant::Level(2)));
        let found = set.draw_stamp_for(a).expect("stamped");
        assert_eq!(found.shown, (1000, 800));
    }
}
