//! YUV -> RGB video shader, compiled to SPIR-V.
//!
//! Given planar luma/chroma textures in a broadcast YCbCr encoding, the fragment
//! shader samples the planes, undoes the studio-range packing, applies the BT.601 or
//! BT.709 YCbCr->RGB matrix, and converts to the render target's transfer. `fs` takes
//! one bilinear tap. `fs_hq` sums a factor-aware Mitchell kernel in linear light so a
//! video shrunk far below its native size does not shimmer.
//!
//! Sources for the constants and formulas:
//! - BT.709 matrix and studio ("limited") quantization: ITU-R BT.709.
//!   https://www.itu.int/rec/R-REC-BT.709
//! - BT.601 matrix and quantization: ITU-R BT.601. https://www.itu.int/rec/R-REC-BT.601
//! - YCbCr <-> RGB coefficients derived from the luma weights (Kr, Kb):
//!   https://en.wikipedia.org/wiki/YCbCr#YCbCr,_4:4:4_to_RGB_conversion
//! - sRGB transfer function and its breakpoints: IEC 61966-2-1.
//!   https://en.wikipedia.org/wiki/SRGB#Transfer_function_(%22gamma%22)
//! - Downscale averaged in LINEAR light, not the gamma-encoded signal, which
//!   otherwise darkens high-contrast edges: mpv's `linear-downscaling`.
//!   https://mpv.io/manual/master/#options-linear-downscaling
//! - The Mitchell-Netravali cubic weights live in `scryglass-shader-common`, which
//!   cites the 1988 paper. `fs_hq` uses B = C = 1/3.

#![no_std]
// spirv-std's macros expand to cfg(target_arch = "spirv"), unknown off-target.
#![expect(unexpected_cfgs)]

use scryglass_shader_common::cubic_weight;
use spirv_std::glam::{UVec4, Vec2, Vec3, Vec4};
use spirv_std::image::Image2d;
use spirv_std::{Sampler, spirv};

#[repr(C)]
pub struct Uniforms {
    dst_min: Vec2,
    dst_max: Vec2,
    src_min: Vec2,
    src_max: Vec2,
    // x = matrix (0 = BT.601, 1 = BT.709)
    // y = full range (0 = studio/limited, 1 = full/PC)
    // z = sRGB target (1 = convert to linear before output)
    // w = chroma format (0 = I420 planar, 1 = NV12 interleaved)
    flags: UVec4,
    // Downscale ratio in luma texels per output pixel, per axis (read by `fs_hq`).
    // <= 1 means no minification, so the kernel collapses to one tap. Chroma needs no
    // separate footprint: its half-res plane and half footprint cancel in the shared
    // normalized UV offset.
    footprint: Vec2,
    // The luma plane's size in texels, to step the kernel taps by whole texels.
    tex_size: Vec2,
}

// rust-gpu reserves set 0, so bindings start at set 1.
#[spirv(vertex)]
pub fn vs(
    #[spirv(vertex_index)] vid: u32,
    #[spirv(uniform, descriptor_set = 1, binding = 0)] uni: &Uniforms,
    #[spirv(position)] out_pos: &mut Vec4,
    out_uv: &mut Vec2,
) {
    let corners = [
        Vec2::new(0.0, 0.0), // first triangle
        Vec2::new(1.0, 0.0),
        Vec2::new(0.0, 1.0),
        Vec2::new(0.0, 1.0), // second triangle
        Vec2::new(1.0, 0.0),
        Vec2::new(1.0, 1.0),
    ];
    let c = corners[vid as usize];
    let d = uni.dst_min + (uni.dst_max - uni.dst_min) * c;
    // [0,1] destination -> clip space [-1,1], flipping Y because textures are
    // top-down and clip space is bottom-up.
    *out_pos = Vec4::new(d.x * 2.0 - 1.0, 1.0 - d.y * 2.0, 0.0, 1.0);
    *out_uv = uni.src_min + (uni.src_max - uni.src_min) * c;
}

/// sRGB gamma -> linear light per lane (IEC 61966-2-1), through the scalar
/// transfer shared in `scryglass-shader-common`.
fn srgb_to_linear(c: Vec3) -> Vec3 {
    Vec3::new(
        scryglass_shader_common::srgb_to_linear(c.x),
        scryglass_shader_common::srgb_to_linear(c.y),
        scryglass_shader_common::srgb_to_linear(c.z),
    )
}

/// Linear light -> sRGB gamma, the inverse of [`srgb_to_linear`] (IEC 61966-2-1).
/// Encodes the multi-tap linear-light average for a non-sRGB target, which stores the
/// gamma value as-is.
fn linear_to_srgb(c: Vec3) -> Vec3 {
    Vec3::new(
        scryglass_shader_common::linear_to_srgb(c.x),
        scryglass_shader_common::linear_to_srgb(c.y),
        scryglass_shader_common::linear_to_srgb(c.z),
    )
}

/// Studio/full-range unpack plus the BT.709 or BT.601 YCbCr -> RGB matrix, clamped
/// to the display range. Shared by the single-tap `fs` and the multi-tap `fs_hq`.
/// The coefficients are derived from the luma weights (see the module sources).
fn ycbcr_to_rgb(ys: f32, us: f32, vs: f32, matrix709: bool, studio: bool) -> Vec3 {
    // Chroma is stored centered at 0.5, so shift it to [-0.5, 0.5].
    let mut luma = ys;
    let mut cb = us - 0.5;
    let mut cr = vs - 0.5;
    // Studio/limited range: rescale 8-bit luma [16, 235] and chroma [16, 240] to
    // [0, 1] and [-0.5, 0.5] (widths 219 and 224). Full range needs no rescale.
    if studio {
        luma = (ys - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (us - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (vs - 128.0 / 255.0) * (255.0 / 224.0);
    }
    let rgb = if matrix709 {
        // BT.709 luma weights Kr = 0.2126, Kb = 0.0722 give these coefficients.
        Vec3::new(
            luma + 1.5748 * cr,
            luma - 0.1873 * cb - 0.4681 * cr,
            luma + 1.8556 * cb,
        )
    } else {
        // BT.601 luma weights Kr = 0.299, Kb = 0.114 give these coefficients.
        Vec3::new(
            luma + 1.402 * cr,
            luma - 0.344136 * cb - 0.714136 * cr,
            luma + 1.772 * cb,
        )
    };
    rgb.clamp(Vec3::ZERO, Vec3::ONE)
}

// Taps per axis on each side of center, matching the still shader's kernel.
const HALF: i32 = 6;

#[spirv(fragment)]
pub fn fs(
    #[spirv(uniform, descriptor_set = 1, binding = 0)] uni: &Uniforms,
    #[spirv(descriptor_set = 1, binding = 1)] tex_y: &Image2d,
    #[spirv(descriptor_set = 1, binding = 2)] tex_u: &Image2d,
    #[spirv(descriptor_set = 1, binding = 3)] tex_v: &Image2d,
    #[spirv(descriptor_set = 1, binding = 4)] samp: &Sampler,
    uv: Vec2,
    output: &mut Vec4,
) {
    let y_tex: Vec4 = tex_y.sample(*samp, uv);
    let u_tex: Vec4 = tex_u.sample(*samp, uv);
    let v_tex: Vec4 = tex_v.sample(*samp, uv);
    // NV12 packs U and V into tex_u.xy. I420 keeps them in separate planes.
    let vs = if uni.flags.w == 1 { u_tex.y } else { v_tex.x };

    let mut rgb = ycbcr_to_rgb(y_tex.x, u_tex.x, vs, uni.flags.x == 1, uni.flags.y == 0);
    // The matrix output is gamma-encoded (sRGB) RGB. An sRGB render target re-encodes
    // linear -> sRGB on write, so feed it linear values to cancel that out. A linear
    // target takes the gamma values as-is.
    if uni.flags.z == 1 {
        rgb = srgb_to_linear(rgb);
    }
    *output = rgb.extend(1.0);
}

/// Factor-aware video downscale: the still shader's widened Mitchell kernel summed
/// over the YCbCr planes, so a video shrunk far below native size stops shimmering.
/// Each tap goes through the matrix and sRGB decode before weighting in LINEAR
/// light, because averaging the gamma-encoded signal darkens high-contrast edges
/// (see the module sources on linear downscaling). At native size or above this
/// collapses to one tap. Mitchell only: no ringing on moving, compressed content.
#[spirv(fragment)]
pub fn fs_hq(
    #[spirv(uniform, descriptor_set = 1, binding = 0)] uni: &Uniforms,
    #[spirv(descriptor_set = 1, binding = 1)] tex_y: &Image2d,
    #[spirv(descriptor_set = 1, binding = 2)] tex_u: &Image2d,
    #[spirv(descriptor_set = 1, binding = 3)] tex_v: &Image2d,
    #[spirv(descriptor_set = 1, binding = 4)] samp: &Sampler,
    uv: Vec2,
    output: &mut Vec4,
) {
    let footprint = uni.footprint;
    let matrix709 = uni.flags.x == 1;
    let studio = uni.flags.y == 0;
    let srgb = uni.flags.z == 1;
    let nv12 = uni.flags.w == 1;

    let rgb_lin = if footprint.x.max(footprint.y) <= 1.0 {
        // No minification: one tap, converted to linear like `fs` does for an sRGB
        // target.
        let y_tex: Vec4 = tex_y.sample(*samp, uv);
        let u_tex: Vec4 = tex_u.sample(*samp, uv);
        let v_tex: Vec4 = tex_v.sample(*samp, uv);
        let vs = if nv12 { u_tex.y } else { v_tex.x };
        srgb_to_linear(ycbcr_to_rgb(y_tex.x, u_tex.x, vs, matrix709, studio))
    } else {
        // Sum bilinear taps across the kernel support, scaled to the footprint, each
        // converted to linear light before it is weighted. The cubic reaches 2 source
        // pixels, so the taps span that radius. The chroma planes sample the same UV
        // offsets: their half-res and half footprint cancel.
        let b = 1.0 / 3.0; // Mitchell (B = C = 1/3)
        let inv_tex = Vec2::ONE / uni.tex_size;
        let mut acc = Vec3::ZERO;
        let mut wsum = 0.0;
        let mut jy = -HALF;
        while jy <= HALF {
            let ty = (jy as f32 / HALF as f32) * 2.0;
            let wy = cubic_weight(ty, b, b);
            let off_y = ty * footprint.y;
            let mut jx = -HALF;
            while jx <= HALF {
                let tx = (jx as f32 / HALF as f32) * 2.0;
                let wx = cubic_weight(tx, b, b);
                let off_x = tx * footprint.x;
                let w = wx * wy;
                let suv = uv + Vec2::new(off_x, off_y) * inv_tex;
                let y_tex: Vec4 = tex_y.sample_by_lod(*samp, suv, 0.0);
                let u_tex: Vec4 = tex_u.sample_by_lod(*samp, suv, 0.0);
                let v_tex: Vec4 = tex_v.sample_by_lod(*samp, suv, 0.0);
                let vs = if nv12 { u_tex.y } else { v_tex.x };
                let lin = srgb_to_linear(ycbcr_to_rgb(y_tex.x, u_tex.x, vs, matrix709, studio));
                acc += lin * w;
                wsum += w;
                jx += 1;
            }
            jy += 1;
        }
        acc / wsum
    };

    // A sharper kernel can ring past the display range. Clamp before re-encoding so a
    // negative channel never makes linear_to_srgb's powf produce NaN.
    let rgb_lin = rgb_lin.clamp(Vec3::ZERO, Vec3::ONE);
    // An sRGB render target re-encodes linear -> sRGB on write, so hand it linear. A
    // linear target stores the gamma value, so put the curve back.
    let rgb = if srgb {
        rgb_lin
    } else {
        linear_to_srgb(rgb_lin)
    };
    *output = rgb.extend(1.0);
}
