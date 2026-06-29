//! RGBA image shader, compiled to SPIR-V.
//!
//! Samples a source sub-rect of an RGBA image (the fit/zoom/pan crop) into a
//! destination quad. A shrunk image sums taps under a kernel that widens with the
//! downscale ratio (`footprint`), averaged in linear light. At 1:1 or magnified it
//! takes a single tap, keeping text and pixel art crisp. Mirrors the YUV video
//! shader (`shaders/yuv`) except the sample: RGBA, not YCbCr planes and matrix.
//!
//! Source for the color constant:
//! - sRGB transfer function: IEC 61966-2-1. https://en.wikipedia.org/wiki/SRGB

#![no_std]
// spirv-std's macros expand to cfg(target_arch = "spirv"), unknown off-target.
#![expect(unexpected_cfgs)]

use scryglass_shader_common::{cubic_weight, lanczos3_weight};
use spirv_std::glam::{UVec4, Vec2, Vec3, Vec4};
use spirv_std::image::Image2d;
use spirv_std::{Sampler, spirv};

#[repr(C)]
pub struct Uniforms {
    dst_min: Vec2,
    dst_max: Vec2,
    src_min: Vec2,
    src_max: Vec2,
    // x = sRGB target (1 = convert to linear before output)
    // y = kernel (0 = single bilinear tap, 1 = cubic(bc), 2 = Lanczos3)
    // z, w unused
    flags: UVec4,
    // Downscale ratio in source texels per output pixel, per axis. <= 1 means no
    // minification, so the kernel collapses to one tap.
    footprint: Vec2,
    // The sampled texture's size in texels, to step taps by whole texels.
    tex_size: Vec2,
    // Mitchell-Netravali (B, C) for the cubic kernel. Ignored by the others.
    bc: Vec2,
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

/// sRGB gamma -> linear light, per IEC 61966-2-1 (see the module source).
fn srgb_to_linear(c: Vec3) -> Vec3 {
    let lo = c / 12.92;
    let hi = ((c + Vec3::splat(0.055)) / 1.055).powf(2.4);
    Vec3::new(
        if c.x <= 0.04045 { lo.x } else { hi.x },
        if c.y <= 0.04045 { lo.y } else { hi.y },
        if c.z <= 0.04045 { lo.z } else { hi.z },
    )
}

/// Linear light -> sRGB gamma, the inverse of [`srgb_to_linear`]. Needed only for a
/// linear render target, which stores the encoded value the multi-tap average is in.
fn linear_to_srgb(c: Vec3) -> Vec3 {
    let lo = c * 12.92;
    let hi = c.powf(1.0 / 2.4) * 1.055 - Vec3::splat(0.055);
    Vec3::new(
        if c.x <= 0.003_130_8 { lo.x } else { hi.x },
        if c.y <= 0.003_130_8 { lo.y } else { hi.y },
        if c.z <= 0.003_130_8 { lo.z } else { hi.z },
    )
}

/// One kernel weight at normalized distance `t`, dispatched by the flags selector.
fn weight(kernel: u32, t: f32, bc: Vec2) -> f32 {
    if kernel == 2 {
        lanczos3_weight(t)
    } else {
        cubic_weight(t, bc.x, bc.y)
    }
}

// Taps per axis on each side of center. The widened kernel is sampled at a fixed
// count of positions across its support, so cost is bounded no matter the ratio.
// Past a heavy downscale the taps spread out and soften rather than fully alias.
// This covers the common photo-at-fit range, keeping the full-res view and its
// demoted view-res copy indistinguishable.
const HALF: i32 = 6;

#[spirv(fragment)]
pub fn fs(
    #[spirv(uniform, descriptor_set = 1, binding = 0)] uni: &Uniforms,
    #[spirv(descriptor_set = 1, binding = 1)] tex: &Image2d,
    #[spirv(descriptor_set = 1, binding = 2)] samp: &Sampler,
    uv: Vec2,
    output: &mut Vec4,
) {
    let srgb = uni.flags.x == 1;
    let kernel = uni.flags.y;
    let footprint = uni.footprint;

    // Single tap when told to (Bilinear), or when there is nothing to minify: at
    // 1:1 a bilinear fetch at the texel center returns the exact texel, so text and
    // pixel art stay crisp, and magnification keeps the plain bilinear upscale.
    if kernel == 0 || footprint.x.max(footprint.y) <= 1.0 {
        let texel: Vec4 = tex.sample(*samp, uv);
        let mut rgb = Vec3::new(texel.x, texel.y, texel.z);
        // The image is sRGB-encoded. An sRGB render target re-encodes linear -> sRGB
        // on write, so feed it linear to cancel that out. A linear target takes the
        // encoded value as-is. Alpha is linear either way, so pass it through.
        if srgb {
            rgb = srgb_to_linear(rgb);
        }
        *output = rgb.extend(texel.w);
        return;
    }

    // Factor-aware downscale: sum bilinear taps across the widened kernel support,
    // averaging in linear light with premultiplied alpha so transparent texels never
    // bleed color into the result. The cubics reach 2 source pixels, Lanczos3 reaches
    // 3, so the taps span that radius scaled by the footprint.
    let radius = if kernel == 2 { 3.0 } else { 2.0 };
    let inv_tex = Vec2::ONE / uni.tex_size;
    let bc = uni.bc;
    let mut acc = Vec3::ZERO; // sum of weight * alpha * linear-rgb
    let mut acc_a = 0.0; // sum of weight * alpha
    let mut wsum = 0.0; // sum of weight

    let mut jy = -HALF;
    while jy <= HALF {
        let ty = (jy as f32 / HALF as f32) * radius;
        let wy = weight(kernel, ty, bc);
        let off_y = ty * footprint.y;
        let mut jx = -HALF;
        while jx <= HALF {
            let tx = (jx as f32 / HALF as f32) * radius;
            let wx = weight(kernel, tx, bc);
            let off_x = tx * footprint.x;
            let w = wx * wy;

            let suv = uv + Vec2::new(off_x, off_y) * inv_tex;
            let s: Vec4 = tex.sample_by_lod(*samp, suv, 0.0);
            let lin = srgb_to_linear(Vec3::new(s.x, s.y, s.z));
            let wa = w * s.w;
            acc += lin * wa;
            acc_a += wa;
            wsum += w;
            jx += 1;
        }
        jy += 1;
    }

    // Clamp before output: a sharper kernel's ringing can push a channel past the
    // display range, and a negative value would make linear_to_srgb's powf produce
    // NaN. This matches the video shader's post-matrix clamp.
    let rgb_lin = if acc_a > 0.0 {
        (acc / acc_a).clamp(Vec3::ZERO, Vec3::ONE)
    } else {
        Vec3::ZERO
    };
    let alpha = if wsum > 0.0 {
        (acc_a / wsum).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // The averaged value is linear. An sRGB target re-encodes on write, so hand it
    // linear. A linear target needs the encoded value put back.
    let rgb = if srgb { rgb_lin } else { linear_to_srgb(rgb_lin) };
    *output = rgb.extend(alpha);
}
