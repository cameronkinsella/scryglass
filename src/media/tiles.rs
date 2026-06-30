//! Tile and LOD-pyramid math for stills too large for one texture: the image
//! is drawn as a grid of tiles cut from a mip level chosen by zoom, so VRAM
//! scales with the window, not the image. Pure math only. Producing and
//! drawing tiles lives with the store and the surface.

use std::collections::HashMap;

/// Tile side in level pixels. Small enough that a window's worth of tiles is
/// a modest texture set, large enough that per-tile overhead stays trivial.
pub const TILE_SIZE: u32 = 512;

/// Deepest mip level: level 6 is a 64x reduction, coarser than any fit view
/// of an image worth tiling.
pub const MAX_LOD: u32 = 6;

/// A tile's identity within one image's pyramid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub lod: u32,
    pub col: u32,
    pub row: u32,
}

/// The mip level to draw at a zoom. Floor of `log2(1/zoom)`: the chosen
/// level is never coarser than the display, so the leftover shrink (at most
/// 2x) is done by the factor-aware sampler and the result stays sharp. At or
/// above 100% only the native level will do.
pub fn lod_for_zoom(zoom: f32) -> u32 {
    if zoom >= 1.0 || zoom <= 0.0 {
        return 0;
    }
    // Nudge past float wobble so an exact power-of-two zoom lands on its level.
    let level = ((1.0 / zoom as f64).log2() + 1e-9).floor();
    (level as u32).min(MAX_LOD)
}

/// Pixel size of a mip level: the original halved `lod` times, rounding up,
/// never below one pixel.
pub fn level_size(original: (u32, u32), lod: u32) -> (u32, u32) {
    let half = |v: u32| ((v as u64 + (1 << lod) - 1) >> lod).max(1) as u32;
    (half(original.0), half(original.1))
}

/// Tile columns and rows covering a level.
pub fn grid(level: (u32, u32)) -> (u32, u32) {
    (level.0.div_ceil(TILE_SIZE), level.1.div_ceil(TILE_SIZE))
}

/// A tile's pixel rectangle `(x, y, w, h)` within its level. Edge tiles are
/// cut short by the level bounds.
pub fn tile_rect(level: (u32, u32), col: u32, row: u32) -> (u32, u32, u32, u32) {
    let x = col * TILE_SIZE;
    let y = row * TILE_SIZE;
    (x, y, TILE_SIZE.min(level.0 - x), TILE_SIZE.min(level.1 - y))
}

/// Gutter of duplicated neighbor pixels around each tile texture. Near a quad's
/// edges the display kernel taps past its source rect (at most `radius x
/// footprint` plus a bilinear texel, about 7 with Lanczos at the 2x residual
/// the LOD floor allows). Without true neighbor data those taps clamp to the
/// tile border and seam. The standard texture-atlas padding fix:
/// https://download.nvidia.com/developer/NVTextureSuite/Atlas_Tools/Texture_Atlas_Whitepaper.pdf
pub const GUTTER: u32 = 8;

/// What to cut and upload for one tile: the gutter-padded source rectangle
/// (signed, since padding may reach past the image, where sampling clamps to
/// the edge exactly like a single texture would) and the padded output size.
/// The tile's payload sits [`GUTTER`] deep inside on every side.
pub fn production(original: (u32, u32), key: TileKey) -> ((i64, i64, u32, u32), (u32, u32)) {
    let level = level_size(original, key.lod);
    let (x, y, w, h) = tile_rect(level, key.col, key.row);
    let pad = |v: u32| ((v + 2 * GUTTER) as u64) << key.lod;
    let region = (
        ((x as i64) - GUTTER as i64) << key.lod,
        ((y as i64) - GUTTER as i64) << key.lod,
        pad(w) as u32,
        pad(h) as u32,
    );
    (region, (w + 2 * GUTTER, h + 2 * GUTTER))
}

/// The tiles overlapping a visible region, given as the normalized source
/// rectangle `[x0, y0, x1, y1]` a placement shows (the `src` of
/// `SurfacePlacement`). Out-of-range and inverted rects yield nothing.
pub fn visible_tiles(
    src: [f32; 4],
    original: (u32, u32),
    lod: u32,
) -> impl Iterator<Item = TileKey> {
    let level = level_size(original, lod);
    window_tiles(src, level).map(move |(col, row)| TileKey { lod, col, row })
}

/// The tiles of a `level`-sized grid overlapping the normalized window
/// `src`, as (col, row) pairs. The exact-scale layer uses this directly
/// with the resting display size as the grid.
pub fn window_tiles(src: [f32; 4], level: (u32, u32)) -> impl Iterator<Item = (u32, u32)> {
    let (cols, rows) = grid(level);
    let span = |a: f32, b: f32, size: u32, count: u32| -> std::ops::Range<u32> {
        let px0 = (a.max(0.0) as f64 * size as f64).floor();
        let px1 = (b.min(1.0) as f64 * size as f64).ceil();
        if px1 <= px0 {
            return 0..0;
        }
        let first = (px0 as u32) / TILE_SIZE;
        let last = (px1 as u32).div_ceil(TILE_SIZE).min(count);
        first..last
    };
    let col_span = span(src[0], src[2], level.0, cols);
    let row_span = span(src[1], src[3], level.1, rows);
    row_span.flat_map(move |row| col_span.clone().map(move |col| (col, row)))
}

/// A count-capped tile cache: recently used tiles stay, the stalest goes
/// when a new one would exceed the cap. The cap is small (a window's worth
/// of tiles), so eviction is a plain scan rather than an ordered structure.
pub struct TileCache<T> {
    entries: HashMap<TileKey, (u64, T)>,
    clock: u64,
    cap: usize,
}

impl<T> TileCache<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            cap: cap.max(1),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether `key` is resident, without touching its recency.
    pub fn contains(&self, key: TileKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// Fetch a tile, marking it as freshly used.
    pub fn get(&mut self, key: TileKey) -> Option<&T> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(&key).map(|entry| {
            entry.0 = clock;
            &entry.1
        })
    }

    /// Insert (or replace) a tile, evicting the least recently used one when
    /// full.
    pub fn insert(&mut self, key: TileKey, value: T) {
        self.clock += 1;
        self.entries.insert(key, (self.clock, value));
        if self.entries.len() > self.cap
            && let Some(stalest) = self
                .entries
                .iter()
                .min_by_key(|(_, (stamp, _))| *stamp)
                .map(|(key, _)| *key)
        {
            self.entries.remove(&stalest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_halves_with_zoom_and_clamps() {
        assert_eq!(lod_for_zoom(2.0), 0);
        assert_eq!(lod_for_zoom(1.0), 0);
        assert_eq!(lod_for_zoom(0.6), 0);
        assert_eq!(lod_for_zoom(0.5), 1);
        assert_eq!(lod_for_zoom(0.26), 1);
        assert_eq!(lod_for_zoom(0.25), 2);
        assert_eq!(lod_for_zoom(0.01), MAX_LOD);
        assert_eq!(lod_for_zoom(0.0), 0);
    }

    #[test]
    fn level_size_halves_rounding_up() {
        assert_eq!(level_size((10000, 5000), 0), (10000, 5000));
        assert_eq!(level_size((10000, 5000), 2), (2500, 1250));
        assert_eq!(level_size((10001, 5001), 1), (5001, 2501));
        assert_eq!(level_size((10, 10), MAX_LOD), (1, 1));
    }

    #[test]
    fn grid_covers_the_level() {
        assert_eq!(grid((2500, 1250)), (5, 3));
        assert_eq!(grid((512, 512)), (1, 1));
        assert_eq!(grid((513, 1)), (2, 1));
    }

    #[test]
    fn edge_tiles_are_cut_short() {
        let level = (2500, 1250);
        assert_eq!(tile_rect(level, 0, 0), (0, 0, 512, 512));
        assert_eq!(tile_rect(level, 4, 2), (2048, 1024, 452, 226));
    }

    #[test]
    fn production_pads_the_source_region_by_the_gutter() {
        let original = (10000, 5000);
        // Level 2 is 2500x1250. The first tile's 512x512 payload plus an
        // 8 px gutter is a 528x528 output cut from a 4x padded region that
        // reaches 32 px past the top-left edge (sampling clamps there).
        let key = TileKey {
            lod: 2,
            col: 0,
            row: 0,
        };
        let (region, out) = production(original, key);
        assert_eq!(out, (528, 528));
        assert_eq!(region, (-32, -32, 2112, 2112));
        // The edge tile (452x226 payload) pads the same way.
        let key = TileKey {
            lod: 2,
            col: 4,
            row: 2,
        };
        let (region, out) = production(original, key);
        assert_eq!(out, (468, 242));
        assert_eq!(region, (8192 - 32, 4096 - 32, 1872, 968));
    }

    #[test]
    fn visible_tiles_cover_the_shown_region_only() {
        let original = (10000, 5000);
        // The whole image at level 2 (2500x1250) is a 5x3 grid.
        let all: Vec<_> = visible_tiles([0.0, 0.0, 1.0, 1.0], original, 2).collect();
        assert_eq!(all.len(), 15);
        // A centered sliver spanning x 40-60%, y 45-55% touches cols 1-2
        // (level x 1000-1500) and only row 1 (level y 562-688).
        let some: Vec<_> = visible_tiles([0.4, 0.45, 0.6, 0.55], original, 2).collect();
        assert_eq!(
            some,
            vec![
                TileKey {
                    lod: 2,
                    col: 1,
                    row: 1
                },
                TileKey {
                    lod: 2,
                    col: 2,
                    row: 1
                },
            ]
        );
    }

    #[test]
    fn inverted_or_outside_rects_yield_no_tiles() {
        let original = (10000, 5000);
        assert_eq!(visible_tiles([0.6, 0.2, 0.4, 0.8], original, 0).count(), 0);
        assert_eq!(
            visible_tiles([-2.0, -2.0, -1.0, -1.0], original, 0).count(),
            0
        );
    }

    #[test]
    fn oversized_rects_clamp_to_the_grid() {
        let original = (600, 600);
        let all: Vec<_> = visible_tiles([-1.0, -1.0, 2.0, 2.0], original, 0).collect();
        assert_eq!(all.len(), 4); // 600x600 at level 0 is a 2x2 grid
    }

    #[test]
    fn cache_evicts_the_least_recently_used() {
        let key = |col| TileKey {
            lod: 0,
            col,
            row: 0,
        };
        let mut cache = TileCache::new(2);
        cache.insert(key(0), "a");
        cache.insert(key(1), "b");
        // Touch 0 so 1 is stalest, then overflow.
        assert_eq!(cache.get(key(0)), Some(&"a"));
        cache.insert(key(2), "c");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(key(1)), None);
        assert_eq!(cache.get(key(0)), Some(&"a"));
        assert_eq!(cache.get(key(2)), Some(&"c"));
    }

    #[test]
    fn cache_replaces_in_place_without_eviction() {
        let key = TileKey {
            lod: 1,
            col: 0,
            row: 0,
        };
        let mut cache = TileCache::new(2);
        cache.insert(key, 1);
        cache.insert(key, 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(key), Some(&2));
    }
}
