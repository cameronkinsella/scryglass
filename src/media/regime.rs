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
    /// Too large for one texture but decodes within the RAM budget. Will be
    /// tiled from its RAM source; until tiling lands it is capped to the
    /// texture limit like a `Resident` image.
    Tiled,
    /// Decoding at full size would exceed the RAM budget: decode-downscaled
    /// to fit, then treated as one of the other two.
    Clamped,
}

/// Classify from header dimensions. The `Resident` test is per-side (the
/// texture limit binds each dimension); the `Clamped` test is total bytes
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

/// The size an oversized decode is reduced to, or `None` to keep it as is.
/// One aspect-preserving target satisfying both limits at once: the RAM
/// budget (`Clamped`) and, until tiling lands, the per-side texture limit.
pub fn decode_target(size: (u32, u32), texture_max: u32, ram_budget: u64) -> Option<(u32, u32)> {
    let (w, h) = size;
    let mut scale: f64 = 1.0;
    match classify(size, texture_max, ram_budget) {
        Regime::Resident => return None,
        Regime::Tiled => {}
        Regime::Clamped => {
            let budget_px = (ram_budget / BYTES_PER_PIXEL) as f64;
            scale = (budget_px / (w as f64 * h as f64)).sqrt();
        }
    }
    // Interim texture cap; tiling (the Tiled regime proper) lifts it.
    scale = scale.min(texture_max as f64 / w.max(h) as f64);
    // Floors keep both constraints exact: the products only shrink.
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
    fn tiled_images_cap_to_the_texture_limit_for_now() {
        // 10000x5000 fits the budget; the interim cap scales it to 8192 wide.
        assert_eq!(
            decode_target((10000, 5000), 8192, 2 * GB),
            Some((8192, 4096))
        );
    }

    #[test]
    fn clamped_images_fit_the_budget_and_the_texture_limit() {
        // 40000x25000 at a 16 GB budget: the byte clamp alone would leave a
        // 63245-px side, so the texture cap binds instead.
        assert_eq!(
            decode_target((40000, 25000), 8192, 16 * GB),
            Some((8192, 5120))
        );
        // A tiny budget binds harder than the texture cap.
        let target = decode_target((40000, 25000), 8192, 40 * 1_000_000).unwrap();
        assert!(decoded_bytes(target) <= 40 * 1_000_000);
        assert!(target.0 > 1 && target.1 > 1);
        // Aspect is preserved within rounding.
        let aspect = 40000.0 / 25000.0;
        let got = target.0 as f64 / target.1 as f64;
        assert!((got - aspect).abs() < 0.01, "aspect drifted: {got}");
    }

    #[test]
    fn degenerate_budgets_never_produce_zero_dimensions() {
        let target = decode_target((40000, 25000), 8192, 4).unwrap();
        assert!(target.0 >= 1 && target.1 >= 1);
    }
}
