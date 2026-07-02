//! The resident tile pyramid for stills too large for one texture: bounded
//! tile caches, production claims, and the draw stamps the demand pass reads
//! back. The grid math itself lives in `crate::media::tiles`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::media::tiles::{TileCache, TileKey};

use super::resident::Keepalive;
use super::uniforms::UNIFORM_SLOTS;

/// Most tiles a pyramid keeps resident: a 4K viewport's worst-case wanted set
/// (see [`UNIFORM_SLOTS`]) plus headroom, so a demand wave never evicts its own
/// freshly produced tiles. Older tiles drop (freeing their VRAM) and are
/// re-produced from the RAM source on return.
const MAX_CACHED_TILES: usize = 224;

/// Most exact-scale tiles kept resident: the visible set plus pan margin
/// (a 4K viewport at one texel per pixel shows ceil(3840/512+1) x
/// ceil(2160/512+1) = 9x6 = 54). All drop together when the resting size
/// changes.
const MAX_EXACT_TILES: usize = 96;

/// The exact-scale layer: tiles that are byte-exact crops of the one-pass
/// downscale at `target`, keyed by grid position with `lod` fixed at 0.
struct ExactLayer {
    /// The whole-image size the tiles are exact for. (0, 0) before the
    /// first rest.
    target: (u32, u32),
    tiles: TileCache<Keepalive>,
    pending: std::collections::HashMap<TileKey, std::time::Instant>,
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
    /// Exact-scale tiles for the resting display size: byte-exact crops of
    /// the one-pass whole-image downscale, produced for the visible region
    /// only, so a rest costs viewport work instead of whole-image work.
    /// Replaced whole when the resting size changes.
    exact: Mutex<ExactLayer>,
    /// The displayed size the last tiled draw spanned, packed `w << 32 | h`.
    /// The demand pass targets exactly this, so the draw's size test and
    /// the produced tiles can never disagree by a rounding step.
    draw_shown: std::sync::atomic::AtomicU64,
    tiles: Mutex<TileCache<Keepalive>>,
    /// Tiles requested but not yet landed, with their claim time, so a pan or
    /// zoom repeating its demand pass never produces the same tile twice. A
    /// claim whose settle message was lost (its window closed mid-production)
    /// expires rather than blocking the tile for the pyramid's lifetime.
    pending: Mutex<std::collections::HashMap<TileKey, std::time::Instant>>,
    /// The mip level the latest demand pass asked for. A queued production
    /// for another level bails before its resample: a zoom that keeps moving
    /// obsoletes whole waves of tiles, and this is what stops them from
    /// being produced anyway.
    wanted_lod: AtomicU32,
    /// What the last tiled draw actually selected, stamped by `prepare_tiles`
    /// and read by the demand pass, so production always targets the level
    /// the real placement samples. Holds [`DRAW_UNSTAMPED`] before any tiled
    /// draw and [`DRAW_BASE_ONLY`] when the base layer alone sufficed.
    draw_lod: AtomicU32,
    /// The scale factor of the last tiled draw (`f32` bits), so the demand
    /// pass works in the physical pixels of the window actually drawing.
    draw_scale: AtomicU32,
}

/// Most tiles one demand pass may claim: what one frame can draw.
pub const MAX_TILE_DRAWS: usize = UNIFORM_SLOTS as usize - 1;

/// `draw_lod` sentinel: no tiled draw has resolved a level yet.
const DRAW_UNSTAMPED: u32 = u32::MAX;
/// `draw_lod` sentinel: the last draw needed no tiles at all.
const DRAW_BASE_ONLY: u32 = u32::MAX - 1;

/// How long a tile claim blocks re-requests before it is presumed lost. Far
/// past any real produce plus upload, so it only fires for a settle message
/// that will never arrive.
const CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// What the demand pass should do, read back from the last real draw.
pub enum DrawWant {
    /// No tiled draw has happened. The caller derives the level itself.
    Unknown,
    /// The base layer sufficed. No tiles are needed.
    BaseOnly,
    /// The draw sampled this level.
    Level(u32),
}

impl TileSet {
    /// A fresh, empty pyramid for an `original`-sized still with `base` as
    /// its view-quality layer.
    pub(super) fn new(original: (u32, u32), base: Keepalive) -> Self {
        Self {
            original,
            base: Mutex::new(base),
            exact: Mutex::new(ExactLayer {
                target: (0, 0),
                tiles: TileCache::new(MAX_EXACT_TILES),
                pending: std::collections::HashMap::new(),
            }),
            tiles: Mutex::new(TileCache::new(MAX_CACHED_TILES)),
            pending: Mutex::new(std::collections::HashMap::new()),
            wanted_lod: AtomicU32::new(0),
            draw_lod: AtomicU32::new(DRAW_UNSTAMPED),
            draw_scale: AtomicU32::new(1.0f32.to_bits()),
            draw_shown: std::sync::atomic::AtomicU64::new(0),
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

    /// Ready the exact layer for `target`, dropping tiles of any other size
    /// (their VRAM frees off-thread as the keepalives drop).
    pub fn ensure_exact(&self, target: (u32, u32)) {
        if let Ok(mut layer) = self.exact.lock()
            && layer.target != target
        {
            layer.target = target;
            layer.tiles = TileCache::new(MAX_EXACT_TILES);
            layer.pending.clear();
        }
    }

    /// The size the exact layer currently serves.
    pub fn exact_target(&self) -> (u32, u32) {
        self.exact
            .lock()
            .map(|layer| layer.target)
            .unwrap_or((0, 0))
    }

    /// Claim one exact tile for production: false when it is resident, in
    /// flight (unexpired), or the layer moved to another size.
    pub fn try_claim_exact(&self, target: (u32, u32), key: TileKey) -> bool {
        self.exact
            .lock()
            .map(|mut layer| {
                if layer.target != target || layer.tiles.contains(key) {
                    return false;
                }
                match layer.pending.get(&key) {
                    Some(claimed) if claimed.elapsed() < CLAIM_TTL => false,
                    _ => {
                        layer.pending.insert(key, std::time::Instant::now());
                        true
                    }
                }
            })
            .unwrap_or(false)
    }

    /// A production for `target` finished: release its claim and install the
    /// texture, unless the layer moved to another size in the meantime.
    pub fn settle_exact(&self, target: (u32, u32), key: TileKey, texture: Option<Keepalive>) {
        if let Ok(mut layer) = self.exact.lock()
            && layer.target == target
        {
            layer.pending.remove(&key);
            if let Some(texture) = texture {
                layer.tiles.insert(key, texture);
            }
        }
    }

    /// A resident exact tile for `target`, refreshing its recency.
    pub(super) fn exact_get(&self, target: (u32, u32), key: TileKey) -> Option<Keepalive> {
        self.exact.lock().ok().and_then(|mut layer| {
            if layer.target != target {
                return None;
            }
            layer.tiles.get(key).cloned()
        })
    }

    /// The displayed size the last tiled draw spanned.
    pub fn draw_shown(&self) -> (u32, u32) {
        let packed = self.draw_shown.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
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

    /// The scale factor of the last tiled draw.
    pub fn draw_scale(&self) -> f32 {
        f32::from_bits(self.draw_scale.load(Ordering::Relaxed))
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

    /// Record the level the current view wants. Stale productions bail.
    pub fn set_wanted_lod(&self, lod: u32) {
        self.wanted_lod.store(lod, Ordering::Relaxed);
    }

    /// The level the latest demand pass asked for.
    pub fn wanted_lod(&self) -> u32 {
        self.wanted_lod.load(Ordering::Relaxed)
    }

    /// Stamp the scale factor of the tiled draw in progress.
    pub(super) fn stamp_draw_scale(&self, scale: f32) {
        self.draw_scale.store(scale.to_bits(), Ordering::Relaxed);
    }

    /// Stamp the displayed size the tiled draw spans, packed for one read.
    pub(super) fn stamp_draw_shown(&self, shown: (u32, u32)) {
        self.draw_shown.store(
            (u64::from(shown.0) << 32) | u64::from(shown.1),
            Ordering::Relaxed,
        );
    }

    /// Stamp what the tiled draw selected, read back by [`Self::draw_want`].
    pub(super) fn stamp_draw_lod(&self, want: DrawWant) {
        let lod = match want {
            DrawWant::Unknown => DRAW_UNSTAMPED,
            DrawWant::BaseOnly => DRAW_BASE_ONLY,
            DrawWant::Level(lod) => lod,
        };
        self.draw_lod.store(lod, Ordering::Relaxed);
    }

    /// What the last real draw wanted, so demand and draw cannot disagree on
    /// scale or rounding.
    pub fn draw_want(&self) -> DrawWant {
        match self.draw_lod.load(Ordering::Relaxed) {
            DRAW_UNSTAMPED => DrawWant::Unknown,
            DRAW_BASE_ONLY => DrawWant::BaseOnly,
            lod => DrawWant::Level(lod),
        }
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
    fn a_resident_tile_is_never_claimed() {
        let set = set();
        set.insert(key(0, 0), test_keepalive());
        assert!(!set.try_claim(key(0, 0)));
    }

    #[test]
    fn exact_layer_target_swap_drops_stale_tiles() {
        let set = set();
        set.ensure_exact((800, 600));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        assert!(set.exact_get((800, 600), key(0, 0)).is_some());
        set.ensure_exact((400, 300));
        assert_eq!(set.exact_target(), (400, 300));
        assert!(set.exact_get((400, 300), key(0, 0)).is_none());
        // The old size no longer serves either.
        assert!(set.exact_get((800, 600), key(0, 0)).is_none());
    }

    #[test]
    fn settle_exact_after_a_retarget_is_a_no_op() {
        let set = set();
        set.ensure_exact((800, 600));
        assert!(set.try_claim_exact((800, 600), key(0, 0)));
        set.ensure_exact((400, 300));
        set.settle_exact((800, 600), key(0, 0), Some(test_keepalive()));
        assert!(set.exact_get((400, 300), key(0, 0)).is_none());
        // Coming back to the old size finds nothing landed and no claim held.
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
    fn draw_shown_round_trips_through_the_packed_stamp() {
        let set = set();
        assert_eq!(set.draw_shown(), (0, 0));
        set.stamp_draw_shown((3840, 2160));
        assert_eq!(set.draw_shown(), (3840, 2160));
        set.stamp_draw_shown((u32::MAX, 1));
        assert_eq!(set.draw_shown(), (u32::MAX, 1));
    }

    #[test]
    fn draw_scale_round_trips_through_its_stamp() {
        let set = set();
        assert_eq!(set.draw_scale(), 1.0);
        set.stamp_draw_scale(1.5);
        assert_eq!(set.draw_scale(), 1.5);
    }

    #[test]
    fn draw_want_maps_the_lod_sentinels() {
        let set = set();
        assert_eq!(set.draw_lod.load(Ordering::Relaxed), DRAW_UNSTAMPED);
        assert!(matches!(set.draw_want(), DrawWant::Unknown));
        set.stamp_draw_lod(DrawWant::BaseOnly);
        assert_eq!(set.draw_lod.load(Ordering::Relaxed), DRAW_BASE_ONLY);
        assert!(matches!(set.draw_want(), DrawWant::BaseOnly));
        set.stamp_draw_lod(DrawWant::Level(3));
        assert!(matches!(set.draw_want(), DrawWant::Level(3)));
        set.stamp_draw_lod(DrawWant::Unknown);
        assert!(matches!(set.draw_want(), DrawWant::Unknown));
    }
}
