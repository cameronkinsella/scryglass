//! Exact CPU port of the image shader's factor-aware downscale (`shaders/image` `fs`).
//!
//! A prefetched neighbor has no full-res GPU texture to render its view-res copy
//! from, so that copy is built on the CPU. To make it indistinguishable from the
//! on-screen full-res (and from the GPU-baked copy a decay produces), this
//! reproduces the shader byte for byte: the same kernel weights (`shaders/common`),
//! the same fixed 13x13 tap grid, gamma-space bilinear taps with clamp-to-edge,
//! premultiplied-alpha averaging in linear light, and the same sRGB transfer. A
//! plain resize (the old path) averaged sRGB bytes with a fixed Catmull-Rom, which
//! is visibly softer and muddier than the linear-light shader kernel.

use rayon::prelude::*;
use scryglass_shader_common::{cubic_weight, lanczos3_weight};

use crate::config::DownscaleKernel;

/// Taps per axis on each side of center. Matches the shader's `HALF`.
const HALF: i32 = 6;

/// One tap's weight and its source-sample offset from the output texel, in output-UV
/// units (the kernel distance `t` maps to `t / target_size` because the footprint is
/// `source / target`). Zero-weight taps are dropped up front.
struct Tap {
    w: f32,
    uv_off: f32,
}

/// Per-axis taps for `target_len` output texels: the shader's 13 grid positions at
/// `t = (j/HALF) * radius`, weighted by the kernel, minus the zeros (a cubic's
/// integer nodes, Lanczos's every-other tap).
fn axis_taps(selector: u32, bc: [f32; 2], radius: f32, target_len: usize) -> Vec<Tap> {
    (-HALF..=HALF)
        .filter_map(|j| {
            let t = (j as f32 / HALF as f32) * radius;
            let w = if selector == 2 {
                lanczos3_weight(t)
            } else {
                cubic_weight(t, bc[0], bc[1])
            };
            (w != 0.0).then_some(Tap {
                w,
                uv_off: t / target_len as f32,
            })
        })
        .collect()
}

/// sRGB gamma -> linear light, per IEC 61966-2-1. Identical to the shader.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light -> sRGB gamma, the inverse of [`srgb_to_linear`].
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        c.powf(1.0 / 2.4) * 1.055 - 0.055
    }
}

/// Table resolution for `srgb_to_linear` over the gamma [0,1] range. A bilinear tap
/// is fractional, so linear interpolation between entries keeps the per-tap
/// linearize cheap while staying well under a code value of the exact transfer.
const LUT: usize = 4096;

/// Precomputed `srgb_to_linear` table with `LUT + 1` entries, read with linear
/// interpolation. Sampling a fractional gamma value this way avoids a `powf` on
/// every one of the millions of taps.
struct SrgbLut([f32; LUT + 1]);

impl SrgbLut {
    fn new() -> Self {
        let mut table = [0.0; LUT + 1];
        for (i, e) in table.iter_mut().enumerate() {
            *e = srgb_to_linear(i as f32 / LUT as f32);
        }
        Self(table)
    }

    fn get(&self, c: f32) -> f32 {
        let x = c.clamp(0.0, 1.0) * LUT as f32;
        let i = (x as usize).min(LUT - 1);
        let frac = x - i as f32;
        self.0[i] + frac * (self.0[i + 1] - self.0[i])
    }
}

/// Bilinear-sample the sRGB-encoded `pixels` in **gamma space** at output UV
/// `(ux, uy)`, clamping to the edge, matching the GPU's `Linear`/`ClampToEdge`
/// sampler on an `Rgba8Unorm` texture. Returns normalized RGBA in `[0, 1]`.
fn bilinear(pixels: &[u8], w: usize, h: usize, ux: f32, uy: f32) -> [f32; 4] {
    let fx = ux * w as f32 - 0.5;
    let fy = uy * h as f32 - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let (tx, ty) = (fx - x0, fy - y0);
    let cx0 = (x0 as isize).clamp(0, w as isize - 1) as usize;
    let cx1 = (x0 as isize + 1).clamp(0, w as isize - 1) as usize;
    let cy0 = (y0 as isize).clamp(0, h as isize - 1) as usize;
    let cy1 = (y0 as isize + 1).clamp(0, h as isize - 1) as usize;
    let texel = |x: usize, y: usize| {
        let o = (y * w + x) * 4;
        [
            pixels[o] as f32 / 255.0,
            pixels[o + 1] as f32 / 255.0,
            pixels[o + 2] as f32 / 255.0,
            pixels[o + 3] as f32 / 255.0,
        ]
    };
    let (p00, p10, p01, p11) = (
        texel(cx0, cy0),
        texel(cx1, cy0),
        texel(cx0, cy1),
        texel(cx1, cy1),
    );
    let mut out = [0.0; 4];
    for c in 0..4 {
        let top = p00[c] + tx * (p10[c] - p00[c]);
        let bot = p01[c] + tx * (p11[c] - p01[c]);
        out[c] = top + ty * (bot - top);
    }
    out
}

/// Downscale `pixels` (RGBA8, sRGB-encoded, `src` = (w, h)) to `target` = (w, h)
/// exactly as the display/decay shader does for `kernel`. `target` must be no larger
/// than `src` on both axes. Returns the view-res RGBA8, sRGB-encoded.
pub(crate) fn downscale(
    pixels: &[u8],
    src: (u32, u32),
    target: (u32, u32),
    kernel: DownscaleKernel,
) -> Vec<u8> {
    downscale_region(pixels, src, (0, 0, src.0, src.1), target, kernel)
}

/// Downscale one `region` (x, y, w, h in source pixels) of `pixels` to `target`,
/// producing a tile of the same downscale a whole-image pass would give. Taps near
/// the region's border sample the full image, not a crop, so adjacent tiles
/// reassemble without seams and no multi-gigabyte crop is ever copied. The origin
/// is signed: a gutter-padded region may start past the image edge, where samples
/// clamp exactly as a single texture's edge would.
pub(crate) fn downscale_region(
    pixels: &[u8],
    src: (u32, u32),
    region: (i64, i64, u32, u32),
    target: (u32, u32),
    kernel: DownscaleKernel,
) -> Vec<u8> {
    let (sw, sh) = (src.0 as usize, src.1 as usize);
    let (tw, th) = (target.0.max(1) as usize, target.1.max(1) as usize);
    let (selector, bc) = kernel.shader_params();

    // A native-size cut (a level-0 tile) is an exact clamped copy, mirroring
    // the shader's collapse of a unit footprint to one exact tap: no kernel
    // softening at 100 percent, and no kernel cost for the most common tiles.
    // Interior columns are one straight memcpy per row. Only the gutter
    // columns past the image edge replicate pixel by pixel.
    if region.2 as usize == tw && region.3 as usize == th {
        let mut out = vec![0u8; tw * th * 4];
        let x0 = region.0;
        let left = (-x0).clamp(0, tw as i64) as usize;
        let in_end = (sw as i64 - x0).clamp(left as i64, tw as i64) as usize;
        for (oy, row) in out.chunks_mut(tw * 4).enumerate() {
            let sy = (region.1 + oy as i64).clamp(0, sh as i64 - 1) as usize;
            let src_row = &pixels[sy * sw * 4..(sy + 1) * sw * 4];
            for px in row[..left * 4].chunks_mut(4) {
                px.copy_from_slice(&src_row[..4]);
            }
            if in_end > left {
                let sx = (x0 + left as i64) as usize;
                row[left * 4..in_end * 4]
                    .copy_from_slice(&src_row[sx * 4..(sx + in_end - left) * 4]);
            }
            for px in row[in_end * 4..].chunks_mut(4) {
                px.copy_from_slice(&src_row[(sw - 1) * 4..]);
            }
        }
        return out;
    }

    // The region's frame in full-image UV: an output texel at region UV `v`
    // sits at `off + v * span` of the image, and a tap offset in region UV
    // scales by `span`.
    let span_x = region.2 as f32 / src.0 as f32;
    let span_y = region.3 as f32 / src.1 as f32;
    let off_x = region.0 as f32 / src.0 as f32;
    let off_y = region.1 as f32 / src.1 as f32;

    let mut out = vec![0u8; tw * th * 4];

    // Bilinear: a single tap per output texel, exactly like the shader's `kernel == 0`
    // branch (no kernel, no linear light, the bilinear sRGB value straight through).
    if selector == 0 {
        out.par_chunks_mut(tw * 4)
            .enumerate()
            .for_each(|(oy, row)| {
                let vy = off_y + (oy as f32 + 0.5) / th as f32 * span_y;
                for (ox, px) in row.chunks_mut(4).enumerate() {
                    let vx = off_x + (ox as f32 + 0.5) / tw as f32 * span_x;
                    let s = bilinear(pixels, sw, sh, vx, vy);
                    for c in 0..4 {
                        px[c] = (s[c] * 255.0).round() as u8;
                    }
                }
            });
        return out;
    }

    let radius = if selector == 2 { 3.0 } else { 2.0 };
    let mut taps_x = axis_taps(selector, bc, radius, tw);
    let mut taps_y = axis_taps(selector, bc, radius, th);
    for tap in &mut taps_x {
        tap.uv_off *= span_x;
    }
    for tap in &mut taps_y {
        tap.uv_off *= span_y;
    }
    // The kernel is separable, so the grid's weight sum is the product of the axes'.
    let wsum: f32 =
        taps_x.iter().map(|t| t.w).sum::<f32>() * taps_y.iter().map(|t| t.w).sum::<f32>();
    // Built once: tile production calls this per tile, and the table is a
    // constant.
    static SRGB_LUT: std::sync::LazyLock<SrgbLut> = std::sync::LazyLock::new(SrgbLut::new);
    let lut = &*SRGB_LUT;

    out.par_chunks_mut(tw * 4)
        .enumerate()
        .for_each(|(oy, row)| {
            let vy = off_y + (oy as f32 + 0.5) / th as f32 * span_y;
            for (ox, px) in row.chunks_mut(4).enumerate() {
                let vx = off_x + (ox as f32 + 0.5) / tw as f32 * span_x;
                // Sum bilinear taps across the widened kernel support in linear light with
                // premultiplied alpha, so transparent texels never bleed color.
                let mut acc = [0.0f32; 3]; // sum of weight * alpha * linear-rgb
                let mut acc_a = 0.0f32; // sum of weight * alpha
                for ty in &taps_y {
                    let sy = vy + ty.uv_off;
                    for tx in &taps_x {
                        let s = bilinear(pixels, sw, sh, vx + tx.uv_off, sy);
                        let wa = tx.w * ty.w * s[3];
                        acc[0] += lut.get(s[0]) * wa;
                        acc[1] += lut.get(s[1]) * wa;
                        acc[2] += lut.get(s[2]) * wa;
                        acc_a += wa;
                    }
                }
                let alpha = if wsum > 0.0 {
                    (acc_a / wsum).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                for c in 0..3 {
                    let lin = if acc_a > 0.0 {
                        (acc[c] / acc_a).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    px[c] = (linear_to_srgb(lin) * 255.0).round() as u8;
                }
                px[3] = (alpha * 255.0).round() as u8;
            }
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KERNELS: [DownscaleKernel; 4] = [
        DownscaleKernel::Bilinear,
        DownscaleKernel::Mitchell,
        DownscaleKernel::CatmullRom,
        DownscaleKernel::Lanczos3,
    ];

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| rgba).collect()
    }

    #[test]
    fn srgb_transfer_round_trips_and_the_lut_tracks_it() {
        let lut = SrgbLut::new();
        for i in 0..=255 {
            let c = i as f32 / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(c)) - c).abs() < 1e-5);
            assert!(
                (lut.get(c) - srgb_to_linear(c)).abs() < 1e-4,
                "lut off at {c}"
            );
        }
    }

    #[test]
    fn a_solid_color_survives_every_kernel() {
        // A flat field must come back unchanged: the kernel is normalized, and the
        // linear-light round-trip is exact, so only the tap grid and the sRGB LUT are
        // under test here. Alpha is carried through (premultiply then unpremultiply).
        for kernel in KERNELS {
            for c in [
                [10, 128, 240, 255],
                [0, 0, 0, 255],
                [255, 255, 255, 255],
                [64, 64, 64, 200],
            ] {
                let out = downscale(&solid(16, 12, c), (16, 12), (5, 4), kernel);
                assert_eq!(out.len(), 5 * 4 * 4);
                for px in out.chunks(4) {
                    for ch in 0..4 {
                        assert!(
                            (px[ch] as i32 - c[ch] as i32).abs() <= 1,
                            "{kernel:?} channel {ch}: got {} want {}",
                            px[ch],
                            c[ch],
                        );
                    }
                }
            }
        }
    }

    /// A deterministic non-uniform test image: per-pixel gradients with a
    /// diagonal edge, so seams and offsets cannot hide.
    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let edge = if x + y < (w + h) / 2 { 255 } else { 40 };
                px.extend_from_slice(&[
                    (x * 255 / w.max(1)) as u8,
                    (y * 255 / h.max(1)) as u8,
                    edge,
                    255,
                ]);
            }
        }
        px
    }

    #[test]
    fn full_region_matches_the_whole_image_downscale() {
        let src = gradient(32, 24);
        for kernel in KERNELS {
            let whole = downscale(&src, (32, 24), (11, 7), kernel);
            let region = downscale_region(&src, (32, 24), (0, 0, 32, 24), (11, 7), kernel);
            assert_eq!(whole, region, "{kernel:?}");
        }
    }

    #[test]
    fn a_native_size_region_is_an_exact_clamped_copy() {
        // A level-0 tile must be the source bytes themselves, with reads past
        // the image edge replicating it, for every kernel.
        let src = gradient(16, 12);
        for kernel in KERNELS {
            let out = downscale_region(&src, (16, 12), (4, 3, 8, 6), (8, 6), kernel);
            for row in 0..6 {
                let want = &src[((3 + row) * 16 + 4) * 4..((3 + row) * 16 + 12) * 4];
                assert_eq!(&out[row * 8 * 4..(row + 1) * 8 * 4], want, "{kernel:?}");
            }
            // A gutter reaching past the top-left clamps to the edge texel.
            let out = downscale_region(&src, (16, 12), (-2, -2, 4, 4), (4, 4), kernel);
            assert_eq!(&out[..4], &src[..4], "{kernel:?} corner");
            assert_eq!(&out[4..8], &src[..4], "{kernel:?} clamped x");
        }
    }

    #[test]
    fn a_padded_region_strips_to_the_unpadded_result() {
        // A tile's gutter must not disturb its payload: pad the region by one
        // output texel's worth of source (footprint 2), produce, strip the
        // ring, and the payload matches the unpadded tile byte for byte.
        let src = gradient(40, 40);
        for kernel in KERNELS {
            let plain = downscale_region(&src, (40, 40), (8, 8, 24, 24), (12, 12), kernel);
            let padded = downscale_region(&src, (40, 40), (6, 6, 28, 28), (14, 14), kernel);
            for row in 0..12 {
                let inner = &padded[((row + 1) * 14 + 1) * 4..((row + 1) * 14 + 13) * 4];
                assert_eq!(
                    inner,
                    &plain[row * 12 * 4..(row + 1) * 12 * 4],
                    "{kernel:?} row {row}"
                );
            }
        }
    }

    #[test]
    fn a_region_past_the_edge_clamps_like_a_border() {
        // A padded region reaching outside the image must replicate the edge,
        // matching what a single texture's clamp-to-edge sampling would show.
        let src = solid(16, 16, [30, 60, 90, 255]);
        let out = downscale_region(
            &src,
            (16, 16),
            (-8, -8, 24, 24),
            (12, 12),
            DownscaleKernel::Mitchell,
        );
        let want: [i32; 4] = [30, 60, 90, 255];
        for px in out.chunks(4) {
            for ch in 0..4 {
                assert!((px[ch] as i32 - want[ch]).abs() <= 1);
            }
        }
    }

    #[test]
    fn tiles_reassemble_the_whole_downscale_without_seams() {
        // Downscale a 64x32 image 2x whole, then as two 16x16 tiles from the
        // left and right halves. Taps near the split sample the full image in
        // both passes, so the tiles must reproduce the whole result exactly.
        let src = gradient(64, 32);
        for kernel in KERNELS {
            let whole = downscale(&src, (64, 32), (32, 16), kernel);
            let left = downscale_region(&src, (64, 32), (0, 0, 32, 32), (16, 16), kernel);
            let right = downscale_region(&src, (64, 32), (32, 0, 32, 32), (16, 16), kernel);
            for row in 0..16 {
                let want = &whole[row * 32 * 4..(row + 1) * 32 * 4];
                assert_eq!(
                    &left[row * 16 * 4..(row + 1) * 16 * 4],
                    &want[..16 * 4],
                    "{kernel:?} left row {row}"
                );
                assert_eq!(
                    &right[row * 16 * 4..(row + 1) * 16 * 4],
                    &want[16 * 4..],
                    "{kernel:?} right row {row}"
                );
            }
        }
    }

    #[test]
    fn a_fully_transparent_image_stays_transparent() {
        for kernel in KERNELS {
            let out = downscale(&solid(16, 16, [200, 40, 40, 0]), (16, 16), (4, 4), kernel);
            for px in out.chunks(4) {
                assert_eq!(px[3], 0, "{kernel:?}");
            }
        }
    }

    #[test]
    fn averaging_is_in_linear_light_not_gamma() {
        // Half black, half white averaged in linear light is ~0.5 linear = 188 sRGB,
        // versus 128 for a gamma-space average. Downscale a black/white split so the
        // result mixes the two, and confirm it lands in the linear range.
        let mut src = Vec::new();
        for _ in 0..8 {
            for x in 0..8 {
                let v = if x < 4 { 0 } else { 255 };
                src.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let out = downscale(&src, (8, 8), (1, 1), DownscaleKernel::Mitchell);
        assert!(
            out[0] > 150,
            "expected linear-light brightening, got {}",
            out[0]
        );
    }
}
