//! Pure zoom/pan math: no iced state, fully unit-tested.

use iced::Size;

use crate::config::ZoomMode;

/// Compute the "opening" zoom factor for Auto mode.
/// 100% if it fits, otherwise shrink-to-fit. Never scale up.
pub fn auto_zoom(img_w: u32, img_h: u32, vp: Size) -> f32 {
    if img_w == 0 || img_h == 0 {
        return 1.0;
    }
    let fit_w = vp.width / img_w as f32;
    let fit_h = vp.height / img_h as f32;
    let fit = fit_w.min(fit_h);
    fit.min(1.0)
}

/// Compute zoom factor for a given ZoomMode.
pub fn compute_zoom(mode: ZoomMode, img_w: u32, img_h: u32, vp: Size) -> f32 {
    if img_w == 0 || img_h == 0 {
        return 1.0;
    }
    match mode {
        ZoomMode::Auto | ZoomMode::LockZoomRatio => auto_zoom(img_w, img_h, vp),
        ZoomMode::ScaleToWidth => vp.width / img_w as f32,
        ZoomMode::ScaleToHeight => vp.height / img_h as f32,
        ZoomMode::ScaleToFit => {
            let fit_w = vp.width / img_w as f32;
            let fit_h = vp.height / img_h as f32;
            fit_w.min(fit_h)
        }
        ZoomMode::ScaleToFill => {
            let fit_w = vp.width / img_w as f32;
            let fit_h = vp.height / img_h as f32;
            fit_w.max(fit_h)
        }
    }
}

/// Clamp pan offset so the image doesn't scroll past edges.
pub fn clamp_pan(pan: (f32, f32), img_w: f32, img_h: f32, vp: Size) -> (f32, f32) {
    let excess_w = (img_w - vp.width).max(0.0) / 2.0;
    let excess_h = (img_h - vp.height).max(0.0) / 2.0;
    let x = pan.0.clamp(-excess_w, excess_w);
    let y = pan.1.clamp(-excess_h, excess_h);
    (x, y)
}

/// Adjust pan so the source pixel under the cursor stays fixed while zooming.
///
/// * `ratio`: new_zoom / old_zoom.
/// * `d`: cursor offset from the viewport center, in logical pixels.
pub fn pan_for_zoom_toward_cursor(pan: (f32, f32), ratio: f32, d: (f32, f32)) -> (f32, f32) {
    (
        d.0 * (1.0 - ratio) + pan.0 * ratio,
        d.1 * (1.0 - ratio) + pan.1 * ratio,
    )
}

/// Step `zoom` by `dir` whole percentage points, snapping to a whole
/// percent so repeated steps stay exact (0.62 -> 0.63).
pub fn nudge_zoom_percent(zoom: f32, dir: i32, min: f32, max: f32) -> f32 {
    let pct = (zoom * 100.0).round() as i32 + dir;
    (pct as f32 / 100.0).clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    // --- auto_zoom ---

    #[test]
    fn auto_zoom_returns_one_when_image_fits() {
        assert_eq!(auto_zoom(400, 300, VP), 1.0);
    }

    #[test]
    fn auto_zoom_returns_one_at_exact_viewport_size() {
        assert_eq!(auto_zoom(800, 600, VP), 1.0);
    }

    #[test]
    fn auto_zoom_shrinks_to_fit_width() {
        // 1600 wide in an 800 viewport → 0.5, height fits at that scale.
        assert_eq!(auto_zoom(1600, 600, VP), 0.5);
    }

    #[test]
    fn auto_zoom_shrinks_to_fit_height() {
        // 1200 tall in a 600 viewport → 0.5.
        assert_eq!(auto_zoom(800, 1200, VP), 0.5);
    }

    #[test]
    fn auto_zoom_uses_most_constrained_axis() {
        // fit_w = 0.5, fit_h = 0.25 → 0.25.
        assert_eq!(auto_zoom(1600, 2400, VP), 0.25);
    }

    #[test]
    fn auto_zoom_zero_dimension_returns_one() {
        assert_eq!(auto_zoom(0, 100, VP), 1.0);
        assert_eq!(auto_zoom(100, 0, VP), 1.0);
    }

    // --- compute_zoom ---

    #[test]
    fn compute_zoom_auto_never_scales_up() {
        assert_eq!(compute_zoom(ZoomMode::Auto, 400, 300, VP), 1.0);
    }

    #[test]
    fn compute_zoom_lock_ratio_matches_auto_on_open() {
        assert_eq!(
            compute_zoom(ZoomMode::LockZoomRatio, 1600, 600, VP),
            compute_zoom(ZoomMode::Auto, 1600, 600, VP),
        );
    }

    #[test]
    fn compute_zoom_scale_to_width_fills_width() {
        // Scales up: 400 wide → 800 viewport = 2.0.
        assert_eq!(compute_zoom(ZoomMode::ScaleToWidth, 400, 300, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_height_fills_height() {
        // 300 tall → 600 viewport = 2.0.
        assert_eq!(compute_zoom(ZoomMode::ScaleToHeight, 400, 300, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_fit_uses_min_axis() {
        // fit_w = 2.0, fit_h = 6.0 → 2.0 (no overflow).
        assert_eq!(compute_zoom(ZoomMode::ScaleToFit, 400, 100, VP), 2.0);
    }

    #[test]
    fn compute_zoom_scale_to_fill_uses_max_axis() {
        // fit_w = 2.0, fit_h = 6.0 → 6.0 (width overflows).
        assert_eq!(compute_zoom(ZoomMode::ScaleToFill, 400, 100, VP), 6.0);
    }

    #[test]
    fn compute_zoom_zero_dimension_returns_one() {
        assert_eq!(compute_zoom(ZoomMode::ScaleToFill, 0, 100, VP), 1.0);
    }

    // --- clamp_pan ---

    #[test]
    fn clamp_pan_centers_image_smaller_than_viewport() {
        assert_eq!(clamp_pan((50.0, -30.0), 400.0, 300.0, VP), (0.0, 0.0));
    }

    #[test]
    fn clamp_pan_limits_to_half_the_excess() {
        // Image 1000×800 in 800×600: excess/2 = (100, 100).
        assert_eq!(
            clamp_pan((500.0, -500.0), 1000.0, 800.0, VP),
            (100.0, -100.0)
        );
    }

    #[test]
    fn clamp_pan_keeps_in_bounds_pan_unchanged() {
        assert_eq!(clamp_pan((50.0, -50.0), 1000.0, 800.0, VP), (50.0, -50.0));
    }

    #[test]
    fn clamp_pan_clamps_one_axis_independently() {
        // Only width overflows: y is always forced to 0.
        assert_eq!(clamp_pan((500.0, 40.0), 1000.0, 300.0, VP), (100.0, 0.0));
    }

    // --- pan_for_zoom_toward_cursor ---

    #[test]
    fn zoom_toward_cursor_unchanged_at_ratio_one() {
        assert_eq!(
            pan_for_zoom_toward_cursor((30.0, -10.0), 1.0, (100.0, 50.0)),
            (30.0, -10.0)
        );
    }

    #[test]
    fn zoom_toward_centered_cursor_scales_pan() {
        // Cursor at viewport center: pan simply scales with the zoom ratio.
        assert_eq!(
            pan_for_zoom_toward_cursor((30.0, -10.0), 2.0, (0.0, 0.0)),
            (60.0, -20.0)
        );
    }

    #[test]
    fn zoom_toward_cursor_keeps_source_point_fixed() {
        // The source pixel under the cursor must stay under the cursor:
        // (d - pan) / zoom is invariant across the zoom change.
        let (old_zoom, new_zoom) = (1.0_f32, 2.0_f32);
        let pan = (30.0, -10.0);
        let d = (100.0, 50.0);
        let new_pan = pan_for_zoom_toward_cursor(pan, new_zoom / old_zoom, d);
        let before = ((d.0 - pan.0) / old_zoom, (d.1 - pan.1) / old_zoom);
        let after = ((d.0 - new_pan.0) / new_zoom, (d.1 - new_pan.1) / new_zoom);
        assert!((before.0 - after.0).abs() < 1e-4);
        assert!((before.1 - after.1).abs() < 1e-4);
    }

    // --- nudge_zoom_percent ---

    #[test]
    fn nudge_zoom_percent_steps_one_whole_percent() {
        assert!((nudge_zoom_percent(0.62, 1, 0.01, 50.0) - 0.63).abs() < 1e-6);
        assert!((nudge_zoom_percent(0.62, -1, 0.01, 50.0) - 0.61).abs() < 1e-6);
    }

    #[test]
    fn nudge_zoom_percent_rounds_a_fractional_zoom_first() {
        // 62.4% rounds to 62, then +1 lands exactly on 63%.
        assert!((nudge_zoom_percent(0.624, 1, 0.01, 50.0) - 0.63).abs() < 1e-6);
    }

    #[test]
    fn nudge_zoom_percent_clamps_to_bounds() {
        assert_eq!(nudge_zoom_percent(0.01, -1, 0.01, 50.0), 0.01);
        assert_eq!(nudge_zoom_percent(50.0, 1, 0.01, 50.0), 50.0);
    }
}
