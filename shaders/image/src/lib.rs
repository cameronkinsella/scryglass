//! RGBA image shader, compiled to SPIR-V.
//!
//! Places a textured quad at a destination rect and samples a source sub-rect of
//! an RGBA image (the fit/zoom/pan crop), then for sRGB render targets converts
//! the color to linear light so the GPU's sRGB re-encode on write round-trips.
//! The geometry mirrors the YUV video shader (`shaders/yuv`). Only the sample
//! differs: one straight RGBA fetch instead of the YCbCr planes and matrix.
//!
//! Source for the color constant:
//! - sRGB transfer function: IEC 61966-2-1. https://en.wikipedia.org/wiki/SRGB

#![no_std]
// spirv-std's macros expand to cfg(target_arch = "spirv"), unknown off-target.
#![expect(unexpected_cfgs)]

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
    // y, z, w unused
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

/// sRGB gamma -> linear light, per IEC 61966-2-1 (see the module source).
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
    #[spirv(descriptor_set = 1, binding = 1)] tex: &Image2d,
    #[spirv(descriptor_set = 1, binding = 2)] samp: &Sampler,
    uv: Vec2,
    output: &mut Vec4,
) {
    let texel: Vec4 = tex.sample(*samp, uv);
    let mut rgb = Vec3::new(texel.x, texel.y, texel.z);

    // The image is sRGB-encoded. An sRGB render target re-encodes linear -> sRGB
    // on write, so feed it linear to cancel that out. A linear target takes the
    // encoded values as-is. Alpha is linear either way, so pass it through.
    if uni.flags.x == 1 {
        rgb = srgb_to_linear(rgb);
    }
    *output = rgb.extend(texel.w);
}
