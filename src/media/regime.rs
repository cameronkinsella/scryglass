//! Size-regime classification: how an image will be displayed, decided from
//! its dimensions before any pixels move. Three regimes, two thresholds:
//! a per-side texture limit (dimensions) and a RAM budget (decoded bytes).

/// Bytes per decoded pixel: the substrate everything downstream consumes is
/// always RGBA8.
pub const BYTES_PER_PIXEL: u64 = 4;

/// How an image of a given size is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// Fits a single texture: fully resident, factor-aware sampled.
    Resident,
    /// Too large for one texture but decodes within the RAM budget: kept at
    /// full size and displayed through the tile pyramid.
    Tiled,
    /// Decoding at full size would exceed the RAM budget: decode-downscaled
    /// to fit, then treated as one of the other two.
    Clamped,
}

/// Classify from header dimensions. The `Resident` test is per-side (the
/// texture limit binds each dimension). The `Clamped` test is total bytes
/// (RAM does not care about shape). A `Resident` image is never `Clamped`:
/// at the 8192 cap it decodes to at most 268 MB, which the budget dwarfs.
pub fn classify(size: (u32, u32), texture_max: u32, ram_budget: u64) -> Regime {
    let (w, h) = size;
    if w.max(h) <= texture_max {
        Regime::Resident
    } else if decoded_bytes(size) <= ram_budget {
        Regime::Tiled
    } else {
        Regime::Clamped
    }
}

/// What a full decode of `size` costs in RAM.
pub fn decoded_bytes(size: (u32, u32)) -> u64 {
    size.0 as u64 * size.1 as u64 * BYTES_PER_PIXEL
}

/// The plain dimension cap for a consumer that cannot tile (a thumbnail
/// decode): the largest aspect-preserving fit within `max` per side, or
/// `None` when it already fits.
pub fn fit_within(size: (u32, u32), max: u32) -> Option<(u32, u32)> {
    let (w, h) = size;
    if w.max(h) <= max {
        return None;
    }
    let scale = max as f64 / w.max(h) as f64;
    Some((
        (((w as f64) * scale).floor() as u32).max(1),
        (((h as f64) * scale).floor() as u32).max(1),
    ))
}

/// The size an oversized decode is reduced to, or `None` to keep it as is.
/// Only a `Clamped` decode is reduced, to the largest aspect-preserving size
/// within the RAM budget.
pub fn decode_target(size: (u32, u32), texture_max: u32, ram_budget: u64) -> Option<(u32, u32)> {
    let (w, h) = size;
    let scale = match classify(size, texture_max, ram_budget) {
        Regime::Resident => return None,
        // Kept at full size: displayed through the tile pyramid.
        Regime::Tiled => return None,
        Regime::Clamped => {
            let budget_px = (ram_budget / BYTES_PER_PIXEL) as f64;
            (budget_px / (w as f64 * h as f64)).sqrt()
        }
    };
    // Floors keep the constraint exact: the product only shrinks.
    let target = (
        ((w as f64 * scale).floor() as u32).max(1),
        ((h as f64 * scale).floor() as u32).max(1),
    );
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn classify_splits_on_dimension_then_bytes() {
        // Per-side check: a long strip is Tiled even though its bytes are small.
        assert_eq!(classify((8192, 8192), 8192, 2 * GB), Regime::Resident);
        assert_eq!(classify((8193, 100), 8192, 2 * GB), Regime::Tiled);
        assert_eq!(classify((20000, 2000), 8192, 2 * GB), Regime::Tiled);
        // Bytes check: 1 gigapixel decodes to 4 GB.
        assert_eq!(classify((40000, 25000), 8192, 2 * GB), Regime::Clamped);
        assert_eq!(classify((40000, 25000), 8192, 4 * GB), Regime::Tiled);
    }

    #[test]
    fn resident_images_are_never_resized() {
        assert_eq!(decode_target((8192, 4096), 8192, 2 * GB), None);
        assert_eq!(decode_target((100, 100), 8192, 2 * GB), None);
    }

    #[test]
    fn tiled_images_decode_at_full_size() {
        // Within the budget but past the texture limit: kept whole for the
        // tile pyramid.
        assert_eq!(decode_target((10000, 5000), 8192, 2 * GB), None);
        assert_eq!(decode_target((40000, 25000), 8192, 16 * GB), None);
    }

    #[test]
    fn clamped_images_fit_the_budget() {
        // 40000x25000 decodes to 4 GB. A 2 GB budget halves the pixels.
        let target = decode_target((40000, 25000), 8192, 2 * GB).unwrap();
        assert!(decoded_bytes(target) <= 2 * GB);
        assert!(decoded_bytes(target) > 19 * GB / 10); // no over-shrink
        // A tiny budget still leaves a usable image.
        let target = decode_target((40000, 25000), 8192, 40 * 1_000_000).unwrap();
        assert!(decoded_bytes(target) <= 40 * 1_000_000);
        assert!(target.0 > 1 && target.1 > 1);
        // Aspect is preserved within rounding.
        let aspect = 40000.0 / 25000.0;
        let got = target.0 as f64 / target.1 as f64;
        assert!((got - aspect).abs() < 0.01, "aspect drifted: {got}");
    }

    #[test]
    fn fit_within_caps_per_side_and_keeps_small_images() {
        assert_eq!(fit_within((100, 50), 64), Some((64, 32)));
        assert_eq!(fit_within((50, 100), 64), Some((32, 64)));
        assert_eq!(fit_within((64, 64), 64), None);
    }

    #[test]
    fn degenerate_budgets_never_produce_zero_dimensions() {
        let target = decode_target((40000, 25000), 8192, 4).unwrap();
        assert!(target.0 >= 1 && target.1 >= 1);
    }
}
