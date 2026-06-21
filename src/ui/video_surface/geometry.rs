//! Turn the shared still-image display math into the rects the shader
//! needs: a destination rect in normalized widget space and a source rect
//! in texture UV. Video textures are native resolution, so texture space
//! equals original space. Pure, so it unit-tests against `display_math`.

use crate::ui::image_display::{DisplayMath, display_math};

/// Convert the display math for `original`-sized content at the given
/// zoom/pan into `(dst, src)` rects, or None when there is nothing to draw.
pub(super) fn geometry(
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    original: (u32, u32),
) -> Option<([f32; 4], [f32; 4])> {
    let (vw, vh) = viewport;
    let (tw, th) = (original.0 as f32, original.1 as f32);
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }

    // Centered destination rect for a shown size in logical pixels.
    let centered = |shown_w: f32, shown_h: f32| {
        let x0 = (vw - shown_w) / 2.0 / vw;
        let y0 = (vh - shown_h) / 2.0 / vh;
        [x0, y0, x0 + shown_w / vw, y0 + shown_h / vh]
    };

    match display_math(zoom, pan, viewport, original, original) {
        DisplayMath::Empty => None,
        DisplayMath::Fit { scale_factor } => {
            let contain = (vw / tw).min(vh / th);
            let dst = centered(tw * contain * scale_factor, th * contain * scale_factor);
            Some((dst, [0.0, 0.0, 1.0, 1.0]))
        }
        DisplayMath::Crop { rect } => {
            let (rw, rh) = (rect.width as f32, rect.height as f32);
            let contain = (vw / rw).min(vh / rh);
            let dst = centered(rw * contain, rh * contain);
            let src = [
                rect.x as f32 / tw,
                rect.y as f32 / th,
                (rect.x as f32 + rw) / tw,
                (rect.y as f32 + rh) / th,
            ];
            Some((dst, src))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f32, f32) = (800.0, 600.0);

    fn assert_rects(got: Option<([f32; 4], [f32; 4])>, dst: [f32; 4], src: [f32; 4]) {
        let (g_dst, g_src) = got.expect("expected geometry, got None");
        for (a, b) in g_dst.iter().zip(dst.iter()) {
            assert!((a - b).abs() < 1e-5, "dst {g_dst:?} != {dst:?}");
        }
        for (a, b) in g_src.iter().zip(src.iter()) {
            assert!((a - b).abs() < 1e-5, "src {g_src:?} != {src:?}");
        }
    }

    #[test]
    fn zero_viewport_draws_nothing() {
        assert_eq!(geometry(1.0, (0.0, 0.0), (0.0, 600.0), (400, 300)), None);
        assert_eq!(geometry(1.0, (0.0, 0.0), (800.0, 0.0), (400, 300)), None);
    }

    #[test]
    fn degenerate_display_math_draws_nothing() {
        // zoom <= 0 makes display_math Empty.
        assert_eq!(geometry(0.0, (0.0, 0.0), VP, (400, 300)), None);
    }

    #[test]
    fn fit_centers_the_whole_texture() {
        // 400x300 at 100% in 800x600 fits; centered at quarter insets, full
        // texture sampled.
        assert_rects(
            geometry(1.0, (0.0, 0.0), VP, (400, 300)),
            [0.25, 0.25, 0.75, 0.75],
            [0.0, 0.0, 1.0, 1.0],
        );
    }

    #[test]
    fn crop_fills_the_viewport_and_maps_the_window_to_uv() {
        // 2000x1000 at 100% in 800x600 overflows: an 800x600 source window,
        // centered (x=600, y=200), fills the viewport and samples a sub-rect.
        assert_rects(
            geometry(1.0, (0.0, 0.0), VP, (2000, 1000)),
            [0.0, 0.0, 1.0, 1.0],
            [0.3, 0.2, 0.7, 0.8],
        );
    }

    #[test]
    fn pan_shifts_the_sampled_window() {
        // Positive pan.x moves the window left, so the source rect starts
        // earlier in x than the centered case.
        let (_, src) = geometry(1.0, (100.0, 0.0), VP, (2000, 1000)).expect("geometry");
        assert!(src[0] < 0.3, "panned u0 {} should be left of 0.3", src[0]);
        assert!(src[0] >= 0.0);
    }
}
