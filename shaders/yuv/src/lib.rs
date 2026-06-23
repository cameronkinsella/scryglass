//! YUV -> RGB video shader, compiled to SPIR-V.
//!
//! Given planar luma/chroma textures in a broadcast YCbCr encoding, the fragment
//! shader samples the planes, undoes the studio-range packing, applies the
//! BT.601 or BT.709 YCbCr->RGB matrix, and (for sRGB targets) converts to linear
//! light.
//!
//! Sources for the color constants:
//! - YCbCr->RGB matrices and studio ranges: ITU-R BT.601 / BT.709.
//!   Coefficient derivation from the luma weights: https://en.wikipedia.org/wiki/YCbCr
//! - sRGB transfer function: IEC 61966-2-1. https://en.wikipedia.org/wiki/SRGB

#![no_std]

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

/// sRGB gamma -> linear light, per IEC 61966-2-1 (see the module sources).
/// Used only for sRGB render targets, where the GPU re-encodes on write.
fn srgb_to_linear(c: Vec3) -> Vec3 {
    let lo = c / 12.92;
    let hi = ((c + Vec3::splat(0.055)) / 1.055).powf(2.4);
    Vec3::new(
        if c.x <= 0.04045 { lo.x } else { hi.x },
        if c.y <= 0.04045 { lo.y } else { hi.y },
        if c.z <= 0.04045 { lo.z } else { hi.z },
    )
}

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
    let ys: Vec4 = tex_y.sample(*samp, uv);
    let u_tex: Vec4 = tex_u.sample(*samp, uv);
    let v_tex: Vec4 = tex_v.sample(*samp, uv);
    let ys = ys.x;
    let us = u_tex.x;
    // NV12 packs U and V into tex_u.xy. I420 keeps them in separate planes.
    let vs = if uni.flags.w == 1 { u_tex.y } else { v_tex.x };

    // Chroma is stored centered at 0.5, so shift it to [-0.5, 0.5].
    let mut luma = ys;
    let mut cb = us - 0.5;
    let mut cr = vs - 0.5;
    // Studio/limited range: rescale 8-bit luma [16, 235] and chroma [16, 240] to
    // [0, 1] and [-0.5, 0.5] (widths 219 and 224). flags.y == 1 is full range.
    if uni.flags.y == 0 {
        luma = (ys - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (us - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (vs - 128.0 / 255.0) * (255.0 / 224.0);
    }

    // YCbCr -> RGB with the BT.709 or BT.601 coefficients, derived from the luma
    // weights (see the module sources).
    let mut rgb = if uni.flags.x == 1 {
        // BT.709
        Vec3::new(
            luma + 1.5748 * cr,
            luma - 0.1873 * cb - 0.4681 * cr,
            luma + 1.8556 * cb,
        )
    } else {
        // BT.601
        Vec3::new(
            luma + 1.402 * cr,
            luma - 0.344136 * cb - 0.714136 * cr,
            luma + 1.772 * cb,
        )
    };
    rgb = rgb.clamp(Vec3::ZERO, Vec3::ONE);

    // The matrix output is gamma-encoded (sRGB) RGB. An sRGB render target
    // re-encodes linear -> sRGB on write, so feed it linear values to cancel
    // that out. A linear target takes the gamma values as-is.
    if uni.flags.z == 1 {
        rgb = srgb_to_linear(rgb);
    }
    *output = rgb.extend(1.0);
}
