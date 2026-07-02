//! Draw-time geometry shared by the still, video, and blur surfaces.
//!
//! Zoom and pan operate in *original* pixel space (the image's true
//! dimensions), while the texture may be a downscaled version of a huge
//! image. The crop rectangle is mapped from original space into texture
//! space at the end, so the same math drives full-resolution images,
//! capped giants, and (later) low-res placeholders identically.

use iced::Rectangle;

/// How to render the texture for a given zoom/pan state.
#[derive(Debug, PartialEq)]
pub(crate) enum DisplayMath {
    /// Image is invalid/degenerate, render nothing.
    Empty,
    /// The zoomed image fits in the viewport: scale the whole texture.
    Fit { scale_factor: f32 },
    /// The zoomed image overflows: crop a window of the texture.
    Crop { rect: Rectangle<u32> },
}

/// Pure display math: decides between fit and crop and computes the
/// numbers, mapping from original-pixel space to texture space.
pub(crate) fn display_math(
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    original: (u32, u32),
    texture: (u32, u32),
) -> DisplayMath {
    let img_w = original.0 as f32;
    let img_h = original.1 as f32;
    let tex_w = texture.0 as f32;
    let tex_h = texture.1 as f32;
    let (vp_w, vp_h) = viewport;

    if img_w <= 0.0 || img_h <= 0.0 || tex_w <= 0.0 || tex_h <= 0.0 || zoom <= 0.0 {
        return DisplayMath::Empty;
    }

    // The zoomed image size in logical pixels (original space).
    let zoomed_w = img_w * zoom;
    let zoomed_h = img_h * zoom;

    // The zoomed image fits the viewport: no crop, scale the whole
    // texture to the zoomed size.
    if zoomed_w <= vp_w && zoomed_h <= vp_h {
        // ContentFit::Contain in a Fill layout shows the texture at
        // min(vp/tex) scale, so correct it to hit the target size.
        let contain = (vp_w / tex_w).min(vp_h / tex_h);
        let shown_w = tex_w * contain;
        let scale_factor = if shown_w > 0.0 {
            zoomed_w / shown_w
        } else {
            1.0
        };
        return DisplayMath::Fit { scale_factor };
    }

    // --- Crop-based zoom & pan (in original space) ---
    //
    // The visible window in source pixels: viewport / zoom.
    let view_src_w = (vp_w / zoom).min(img_w);
    let view_src_h = (vp_h / zoom).min(img_h);

    // Center of the visible window. Pan is in logical (screen) pixels,
    // so convert to source pixels by dividing by zoom.
    let cx = img_w / 2.0 - pan.0 / zoom;
    let cy = img_h / 2.0 - pan.1 / zoom;

    // Top-left corner of the crop rectangle, clamped to valid range.
    let crop_x = (cx - view_src_w / 2.0).clamp(0.0, img_w - view_src_w);
    let crop_y = (cy - view_src_h / 2.0).clamp(0.0, img_h - view_src_h);

    // Map from original space into texture space.
    let sx = tex_w / img_w;
    let sy = tex_h / img_h;

    DisplayMath::Crop {
        rect: Rectangle {
            x: (crop_x * sx).round() as u32,
            y: (crop_y * sy).round() as u32,
            width: ((view_src_w * sx).round() as u32).max(1),
            height: ((view_src_h * sy).round() as u32).max(1),
        },
    }
}

/// Convert the display math for `original`-sized content at the given zoom/pan
/// into normalized destination and source-UV rects for a GPU shader (the still
/// and video surfaces), or None when there is nothing to draw. The shader
/// samples in UV space, so the texture's own pixel size does not enter here.
pub(crate) fn display_geometry(
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

/// The downscale ratio in source texels per output pixel, per axis, for content
/// placed at `dst` (normalized widget space) sampling `src` (texture UV) of a
/// `tex_size`-texel texture into a `viewport`-pixel area. Above 1 means that axis
/// is minified, so the shader widens its kernel to that footprint. At 1:1 the
/// shader takes a single tap. Exact, so it never leans on screen-space derivatives.
pub(crate) fn footprint(
    dst: [f32; 4],
    src: [f32; 4],
    tex_size: (u32, u32),
    viewport: (f32, f32),
) -> [f32; 2] {
    let (tw, th) = (tex_size.0 as f32, tex_size.1 as f32);
    let (vw, vh) = viewport;
    let dst_px_w = (dst[2] - dst[0]) * vw;
    let dst_px_h = (dst[3] - dst[1]) * vh;
    let src_tx_w = (src[2] - src[0]) * tw;
    let src_tx_h = (src[3] - src[1]) * th;
    [
        if dst_px_w > 0.0 {
            src_tx_w / dst_px_w
        } else {
            1.0
        },
        if dst_px_h > 0.0 {
            src_tx_h / dst_px_h
        } else {
            1.0
        },
    ]
}

/// How far above 1.0 a footprint still counts as 1:1. A demoted view-res copy is
/// baked to the display size, so rounding leaves its footprint a hair over 1. This
/// is well below any real downscale ratio, so a genuine shrink still gets the kernel.
const NEAR_ONE_EPS: f32 = 0.03;

/// Snap a footprint within [`NEAR_ONE_EPS`] above 1.0 down to exactly 1.0, so a
/// view-res copy shown at its baked size takes the single exact tap instead of a
/// redundant kernel pass that would soften it. Leaves magnification and real
/// downscales untouched.
pub(crate) fn snap_footprint_to_unit(footprint: f32) -> f32 {
    if footprint <= 1.0 + NEAR_ONE_EPS {
        footprint.min(1.0)
    } else {
        footprint
    }
}

/// Whether both axes sit within [`NEAR_ONE_EPS`] of 1:1, so the surface is a
/// near-exact copy (a demoted view-res at its baked size, or a 100%-zoom image)
/// that should be pixel-snapped rather than a real min/magnification that keeps its
/// kernel. Footprints past 1:1 are already snapped down, so this excludes them.
pub(crate) fn near_one_to_one(footprint: [f32; 2]) -> bool {
    (footprint[0] - 1.0).abs() <= NEAR_ONE_EPS && (footprint[1] - 1.0).abs() <= NEAR_ONE_EPS
}

/// Snap one axis so the display spans a whole number of physical pixels with its
/// left/top on a pixel boundary, keeping the center. That puts every surface on
/// one pixel grid. When `align_src`, also move the source window to cover exactly
/// that many texels from a texel boundary, so each pixel center lands on a texel
/// center and the single tap is exact (the near-1:1 copy). Otherwise the source
/// is left as is (a real min/magnification keeps sampling the same span). Returns
/// the new `(dst_start, dst_end, src_start, src_end)` in normalized units.
fn snap_axis(
    d0: f32,
    d1: f32,
    s0: f32,
    s1: f32,
    phys: f32,
    tex: f32,
    align_src: bool,
) -> (f32, f32, f32, f32) {
    // A magnified image is drawn larger than its texture, so its displayed pixel span
    // legitimately exceeds `tex`. Only the near-1:1 copy caps at the texture, so its
    // source window below stays within bounds. Capping a magnification would pin the
    // image to a native-size box and crop it as the zoom grows.
    let want = ((d1 - d0) * phys).round().max(1.0);
    // The texel-exact copy only exists when the pixel span matches the requested
    // source window to within rounding dust. A full-source fit gets no slack
    // beyond rounding: every windowed texel visibly shaves its edges, as when a
    // base or view-res copy sits a hair finer than a shrinking fit. A tidied
    // zoom crop keeps a few texels of slack, since its edges are already
    // mid-content and the pan can reach them.
    let src_texels = (s1 - s0) * tex;
    let full_src = s0 * tex <= 0.5 && (1.0 - s1) * tex <= 0.5;
    let dust = if full_src { 1.0 } else { 3.0 };
    let align = align_src && (src_texels - want).abs() <= dust;
    let pixels = if align { want.min(tex) } else { want };
    let center = (d0 + d1) * 0.5 * phys;
    let a0 = (center - pixels * 0.5).round();
    let (src0, src1) = if align {
        let src_center = (s0 + s1) * 0.5 * tex;
        let t0 = (src_center - pixels * 0.5).round().clamp(0.0, tex - pixels);
        (t0 / tex, (t0 + pixels) / tex)
    } else {
        (s0, s1)
    };
    (a0 / phys, (a0 + pixels) / phys, src0, src1)
}

/// Snap `dst` to whole physical pixels so the surface sits on the pixel grid and a
/// view-res demote never shifts it. When `align_src` (a near-1:1 copy) the source
/// snaps to texel centers too, making the single tap a pixel-exact copy.
/// `physical_viewport` is the image area in physical pixels.
pub(crate) fn snap_placement_to_pixels(
    dst: [f32; 4],
    src: [f32; 4],
    tex_size: (f32, f32),
    physical_viewport: (f32, f32),
    align_src: bool,
) -> ([f32; 4], [f32; 4]) {
    let (pw, ph) = physical_viewport;
    let (tw, th) = tex_size;
    if pw <= 0.0 || ph <= 0.0 || tw <= 0.0 || th <= 0.0 {
        return (dst, src);
    }
    let (dx0, dx1, sx0, sx1) = snap_axis(dst[0], dst[2], src[0], src[2], pw, tw, align_src);
    let (dy0, dy1, sy0, sy1) = snap_axis(dst[1], dst[3], src[1], src[3], ph, th, align_src);
    ([dx0, dy0, dx1, dy1], [sx0, sy0, sx1, sy1])
}

/// Where a shader surface draws its content this frame: the destination and
/// source-UV rects plus the sampling mode, resolved from the display geometry.
/// `valid` is false for the degenerate case that draws nothing. The still and video
/// surfaces both carry this, so their placement stays identical.
#[derive(Clone, Copy)]
pub(crate) struct SurfacePlacement {
    pub valid: bool,
    /// Destination rect in normalized widget space: x0, y0, x1, y1.
    pub dst: [f32; 4],
    /// Source rect in texture UV space: u0, v0, u1, v1.
    pub src: [f32; 4],
}

impl SurfacePlacement {
    pub(crate) fn new(
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        original: (u32, u32),
    ) -> Self {
        match display_geometry(zoom, pan, viewport, original) {
            Some((dst, src)) => Self {
                valid: true,
                dst,
                src,
            },
            None => Self::empty(),
        }
    }

    /// The degenerate placement that draws nothing (the still warmup surface).
    pub(crate) fn empty() -> Self {
        Self {
            valid: false,
            dst: [0.0; 4],
            src: [0.0; 4],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f32, f32) = (800.0, 600.0);

    #[test]
    fn small_image_at_full_zoom_uses_fit() {
        // 400×300 at 100% in 800×600: contain factor = 2.0, so the target
        // on-screen width is 400 → scale_factor = 0.5.
        let math = display_math(1.0, (0.0, 0.0), VP, (400, 300), (400, 300));
        assert_eq!(math, DisplayMath::Fit { scale_factor: 0.5 });
    }

    #[test]
    fn fit_scale_is_resolution_independent() {
        // A half-resolution texture of the same image must produce the
        // same on-screen size: contain factor doubles, scale halves out.
        let full = display_math(1.0, (0.0, 0.0), VP, (400, 300), (400, 300));
        let half = display_math(1.0, (0.0, 0.0), VP, (400, 300), (200, 150));
        match (full, half) {
            (DisplayMath::Fit { scale_factor: a }, DisplayMath::Fit { scale_factor: b }) => {
                // shown_w differs (800 both ways here since both contain to
                // viewport width). Equality of on-screen size is what counts.
                assert!((a - b).abs() < 1e-5, "expected {a} == {b}");
            }
            other => panic!("expected Fit paths, got {other:?}"),
        }
    }

    #[test]
    fn overflowing_zoom_crops_centered_window() {
        // 2000×1000 at 100% in 800×600: window = 800×600 source pixels,
        // centered → x = 600, y = 200.
        let math = display_math(1.0, (0.0, 0.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 600,
                    y: 200,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn pan_shifts_crop_window() {
        // Positive pan.x shifts the image right = window moves left.
        let math = display_math(1.0, (100.0, 0.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 500,
                    y: 200,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn crop_clamps_at_image_edges() {
        let math = display_math(1.0, (10_000.0, 10_000.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn crop_rect_maps_into_downscaled_texture_space() {
        // Same view as overflowing_zoom_crops_centered_window, but the
        // texture is half resolution: every coordinate halves.
        let math = display_math(1.0, (0.0, 0.0), VP, (2000, 1000), (1000, 500));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 300,
                    y: 100,
                    width: 400,
                    height: 300,
                }
            }
        );
    }

    #[test]
    fn degenerate_inputs_are_empty() {
        assert_eq!(
            display_math(0.0, (0.0, 0.0), VP, (100, 100), (100, 100)),
            DisplayMath::Empty
        );
        assert_eq!(
            display_math(1.0, (0.0, 0.0), VP, (0, 100), (0, 100)),
            DisplayMath::Empty
        );
    }

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
    fn geometry_zero_viewport_draws_nothing() {
        assert_eq!(
            display_geometry(1.0, (0.0, 0.0), (0.0, 600.0), (400, 300)),
            None
        );
        assert_eq!(
            display_geometry(1.0, (0.0, 0.0), (800.0, 0.0), (400, 300)),
            None
        );
    }

    #[test]
    fn geometry_degenerate_math_draws_nothing() {
        assert_eq!(display_geometry(0.0, (0.0, 0.0), VP, (400, 300)), None);
    }

    #[test]
    fn geometry_fit_centers_the_whole_texture() {
        assert_rects(
            display_geometry(1.0, (0.0, 0.0), VP, (400, 300)),
            [0.25, 0.25, 0.75, 0.75],
            [0.0, 0.0, 1.0, 1.0],
        );
    }

    #[test]
    fn geometry_crop_fills_the_viewport_and_maps_uv() {
        assert_rects(
            display_geometry(1.0, (0.0, 0.0), VP, (2000, 1000)),
            [0.0, 0.0, 1.0, 1.0],
            [0.3, 0.2, 0.7, 0.8],
        );
    }

    #[test]
    fn geometry_pan_shifts_the_sampled_window() {
        let (_, src) = display_geometry(1.0, (100.0, 0.0), VP, (2000, 1000)).expect("geometry");
        assert!(src[0] < 0.3, "panned u0 {} should be left of 0.3", src[0]);
        assert!(src[0] >= 0.0);
    }

    #[test]
    fn footprint_is_one_at_one_to_one() {
        // A 400x300 texture shown fit in 800x600 lands at exactly its own pixel size.
        let (dst, src) = display_geometry(1.0, (0.0, 0.0), VP, (400, 300)).expect("geometry");
        let fp = footprint(dst, src, (400, 300), VP);
        assert!((fp[0] - 1.0).abs() < 1e-5, "fp.x {}", fp[0]);
        assert!((fp[1] - 1.0).abs() < 1e-5, "fp.y {}", fp[1]);
    }

    #[test]
    fn footprint_grows_with_the_downscale_ratio() {
        // The whole texture (800x600) shown at half size (400x300) is a 2x minify.
        let dst = [0.25, 0.25, 0.75, 0.75];
        let fp = footprint(dst, [0.0, 0.0, 1.0, 1.0], (800, 600), VP);
        assert!((fp[0] - 2.0).abs() < 1e-5, "fp.x {}", fp[0]);
        assert!((fp[1] - 2.0).abs() < 1e-5, "fp.y {}", fp[1]);
    }

    #[test]
    fn footprint_guards_a_degenerate_destination() {
        let fp = footprint([0.5, 0.5, 0.5, 0.5], [0.0, 0.0, 1.0, 1.0], (800, 600), VP);
        assert_eq!(fp, [1.0, 1.0]);
    }

    #[test]
    fn snap_pulls_a_rounding_hair_over_one_to_a_single_tap() {
        // 5333x3000 at 21%: view-res 1120px shown at 1119.93 -> 1.00006 -> 1.0.
        assert_eq!(snap_footprint_to_unit(1120.0 / 1119.93), 1.0);
        assert_eq!(snap_footprint_to_unit(1.02), 1.0);
    }

    #[test]
    fn snap_leaves_magnification_and_real_downscales_alone() {
        assert_eq!(snap_footprint_to_unit(0.5), 0.5);
        assert_eq!(snap_footprint_to_unit(1.0), 1.0);
        assert_eq!(snap_footprint_to_unit(1.5), 1.5);
        assert_eq!(snap_footprint_to_unit(4.76), 4.76);
    }

    #[test]
    fn near_one_to_one_covers_a_view_res_but_not_a_real_scale() {
        // A demoted copy sits a hair either side of 1:1 after rounding.
        assert!(near_one_to_one([1.0, 0.9997]));
        assert!(near_one_to_one([1.0, 1.0]));
        // Fit downscale and magnification keep their kernel.
        assert!(!near_one_to_one([4.76, 4.76]));
        assert!(!near_one_to_one([0.5, 0.5]));
        assert!(!near_one_to_one([1.0, 0.5]));
    }

    #[test]
    fn snap_fit_maps_the_whole_texture_one_to_one() {
        // 200x100 texture shown at ~1:1 (dst spans 200x100 physical px): dst lands on
        // whole pixels spanning exactly the texture, src stays the whole texture.
        let (dst, src) = snap_placement_to_pixels(
            [0.25, 0.25, 0.75, 0.75],
            [0.0, 0.0, 1.0, 1.0],
            (200.0, 100.0),
            (400.0, 200.0),
            true,
        );
        assert!(((dst[2] - dst[0]) * 400.0 - 200.0).abs() < 1e-3);
        assert!(((dst[3] - dst[1]) * 200.0 - 100.0).abs() < 1e-3);
        assert_eq!(src, [0.0, 0.0, 1.0, 1.0]);
        assert!((dst[0] * 400.0).fract().abs() < 1e-3);
    }

    #[test]
    fn snap_without_align_snaps_dst_but_keeps_source() {
        // A real downscale (full-res at fit) still lands on the pixel grid so it
        // shares the demote's position, but its source window is untouched.
        let src_in = [0.0, 0.0, 1.0, 1.0];
        let (dst, src) = snap_placement_to_pixels(
            [0.1035, 0.0, 0.8965, 1.0],
            src_in,
            (5333.0, 3000.0),
            (1415.0, 631.2),
            false,
        );
        assert_eq!(src, src_in);
        assert!((dst[0] * 1415.0).fract().abs() < 1e-3);
        assert!((dst[2] * 1415.0).fract().abs() < 1e-3);
    }

    #[test]
    fn snap_lets_a_magnified_image_grow_past_its_texture() {
        // A 200x100 texture zoomed so its displayed span (300x150 physical px) exceeds
        // the texture must keep that larger size, not clamp back into a native-size box
        // (the zoom regression). Magnification is not a near-1:1 copy, so align_src is
        // false.
        let (dst, _src) = snap_placement_to_pixels(
            [0.25, 0.25, 0.75, 0.75],
            [0.0, 0.0, 1.0, 1.0],
            (200.0, 100.0),
            (600.0, 300.0),
            false,
        );
        assert!(
            ((dst[2] - dst[0]) * 600.0 - 300.0).abs() < 1e-3,
            "x span should stay 300 px, got {}",
            (dst[2] - dst[0]) * 600.0
        );
        assert!(
            ((dst[3] - dst[1]) * 300.0 - 150.0).abs() < 1e-3,
            "y span should stay 150 px, got {}",
            (dst[3] - dst[1]) * 300.0
        );
    }

    #[test]
    fn snap_crop_shows_whole_texels_over_whole_pixels() {
        // A 636-tall texture filling a 631.2px area (a slight overflow) shows a whole
        // 631 texels over 631 pixels, the source snapped to a texel boundary.
        let (dst, src) = snap_placement_to_pixels(
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 0.002, 1.0, 0.998],
            (1131.0, 636.0),
            (1131.0, 631.2),
            true,
        );
        let shown_px = (dst[3] - dst[1]) * 631.2;
        let shown_tex = (src[3] - src[1]) * 636.0;
        assert!((shown_px - 631.0).abs() < 1e-2, "shown_px {shown_px}");
        assert!((shown_tex - 631.0).abs() < 1e-2, "shown_tex {shown_tex}");
        assert!((src[1] * 636.0).fract().abs() < 1e-2);
    }

    #[test]
    fn a_finer_texture_shrinks_unaligned_instead_of_cropping() {
        // A 625x585 exact base under a width-driven fit that wants 615x575 px:
        // inside the near-1:1 band, but aligning would window 615 of the 625
        // texels and shave the edges off a fully-fit image.
        let dst = [0.0, 0.0, 615.0 / 640.0, 575.0 / 600.0];
        let src = [0.0, 0.0, 1.0, 1.0];
        let (sdst, ssrc) = snap_placement_to_pixels(dst, src, (625.0, 585.0), (640.0, 600.0), true);
        assert_eq!(ssrc, src, "the full source must survive");
        let span = (sdst[2] - sdst[0]) * 640.0;
        assert!((span - 615.0).abs() < 0.5, "span {span}");

        // Even a two-texel window off a full-source fit is a visible shave.
        let dst = [0.0, 0.0, 623.0 / 640.0, 583.0 / 600.0];
        let (_, ssrc) = snap_placement_to_pixels(dst, src, (625.0, 585.0), (640.0, 600.0), true);
        assert_eq!(ssrc, src, "two texels of window still crop content");
    }

    #[test]
    fn a_true_copy_still_aligns_to_texels() {
        // Within rounding dust of the texture size the exact copy engages.
        let dst = [0.0, 0.0, 625.4 / 640.0, 585.4 / 600.0];
        let src = [0.0, 0.0, 1.0, 1.0];
        let (sdst, ssrc) = snap_placement_to_pixels(dst, src, (625.0, 585.0), (640.0, 600.0), true);
        assert_eq!(ssrc, src);
        let span = (sdst[2] - sdst[0]) * 640.0;
        assert!((span - 625.0).abs() < 0.5, "span {span}");
    }

    #[test]
    fn snap_placement_guards_degenerate_inputs() {
        let dst = [0.1, 0.1, 0.9, 0.9];
        let src = [0.0, 0.0, 1.0, 1.0];
        assert_eq!(
            snap_placement_to_pixels(dst, src, (200.0, 100.0), (0.0, 200.0), true),
            (dst, src)
        );
        assert_eq!(
            snap_placement_to_pixels(dst, src, (0.0, 0.0), (400.0, 200.0), true),
            (dst, src)
        );
    }
}
