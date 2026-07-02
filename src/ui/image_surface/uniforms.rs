//! Per-draw uniform packing for the still-image shader, plus the pure math
//! that places one tile's rects into a draw's uniform slot.

pub(super) const UNIFORM_SIZE: u64 = 80;

/// Uniform slots one frame can draw: slot 0 for the single texture (or a tile
/// pyramid's base layer), the rest for visible tiles. The LOD floor leaves up
/// to 2 level texels per physical pixel, so a 4K viewport can show
/// ceil(3840*2/512)+1 x ceil(2160*2/512)+1 = 16x10 tiles, 17x11 with the
/// demand margin. 192 covers that with headroom. A larger display degrades to
/// the base layer past the cap.
pub(super) const UNIFORM_SLOTS: u64 = 192;

/// Byte stride between slots: the WebGPU default limit for
/// `minUniformBufferOffsetAlignment`, https://www.w3.org/TR/webgpu/#limits
pub(super) const UNIFORM_STRIDE: u64 = 256;

/// Pack the per-draw uniform block to match the shader `Uniforms` struct (80 bytes):
/// the dst/src rects (0..32), the flags `UVec4` (32..48, x = sRGB, y = kernel), then
/// footprint, tex_size, and the cubic `(B, C)` (48..72). 72..80 is tail padding.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_uniforms(
    dst: [f32; 4],
    src: [f32; 4],
    is_srgb: bool,
    kernel: u32,
    footprint: [f32; 2],
    tex_size: [f32; 2],
    bc: [f32; 2],
) -> [u8; 80] {
    let mut buf = [0u8; 80];
    let floats = [
        dst[0], dst[1], dst[2], dst[3], src[0], src[1], src[2], src[3],
    ];
    for (i, f) in floats.iter().enumerate() {
        buf[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf[32..36].copy_from_slice(&(is_srgb as u32).to_le_bytes());
    buf[36..40].copy_from_slice(&kernel.to_le_bytes());
    let tail = [
        footprint[0],
        footprint[1],
        tex_size[0],
        tex_size[1],
        bc[0],
        bc[1],
    ];
    for (i, f) in tail.iter().enumerate() {
        let o = 48 + i * 4;
        buf[o..o + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf
}

/// Where one tile lands on screen and what part of its padded texture shows:
/// the tile's payload rectangle mapped from level space through the image's
/// placement, and the source rect inset past the gutter.
pub(super) fn tile_placement(
    dst: [f32; 4],
    src: [f32; 4],
    level: (u32, u32),
    key: crate::media::tiles::TileKey,
    tex: (u32, u32),
) -> ([f32; 4], [f32; 4]) {
    use crate::media::tiles::GUTTER;
    let (x, y, w, h) = crate::media::tiles::tile_rect(level, key.col, key.row);
    // The payload's rect in image UV, then through the placement's linear
    // src -> dst map. Adjacent tiles share exact edge coordinates, so the
    // rasterizer leaves no cracks.
    let (lw, lh) = (level.0 as f32, level.1 as f32);
    let map = |v: f32, s0: f32, s1: f32, d0: f32, d1: f32| d0 + (v - s0) / (s1 - s0) * (d1 - d0);
    let tdst = [
        map(x as f32 / lw, src[0], src[2], dst[0], dst[2]),
        map(y as f32 / lh, src[1], src[3], dst[1], dst[3]),
        map((x + w) as f32 / lw, src[0], src[2], dst[0], dst[2]),
        map((y + h) as f32 / lh, src[1], src[3], dst[1], dst[3]),
    ];
    let (tw, th) = (tex.0 as f32, tex.1 as f32);
    let g = GUTTER as f32;
    let tsrc = [g / tw, g / th, (g + w as f32) / tw, (g + h as f32) / th];
    (tdst, tsrc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tiles::TileKey;

    fn u32_at(buf: &[u8; 80], offset: usize) -> u32 {
        u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
    }

    fn f32_at(buf: &[u8; 80], offset: usize) -> f32 {
        f32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
    }

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "{a} vs {b}");
    }

    #[test]
    fn packs_dst_src_flags_footprint_tex_size_then_cubic() {
        let dst = [0.1, 0.2, 0.3, 0.4];
        let src = [0.5, 0.6, 0.7, 0.8];
        let buf = build_uniforms(dst, src, true, 3, [1.5, 2.0], [1024.0, 768.0], [0.25, 0.75]);
        for (i, v) in dst.iter().chain(src.iter()).enumerate() {
            assert_eq!(f32_at(&buf, i * 4), *v);
        }
        assert_eq!(u32_at(&buf, 32), 1, "srgb target");
        assert_eq!(u32_at(&buf, 36), 3, "kernel selector");
        assert_eq!(f32_at(&buf, 48), 1.5, "footprint x");
        assert_eq!(f32_at(&buf, 52), 2.0, "footprint y");
        assert_eq!(f32_at(&buf, 56), 1024.0, "tex width");
        assert_eq!(f32_at(&buf, 60), 768.0, "tex height");
        assert_eq!(f32_at(&buf, 64), 0.25, "cubic B");
        assert_eq!(f32_at(&buf, 68), 0.75, "cubic C");
    }

    #[test]
    fn unused_flag_lanes_and_tail_padding_stay_zero() {
        let buf = build_uniforms(
            [1.0; 4],
            [1.0; 4],
            false,
            0,
            [1.0, 1.0],
            [2.0, 2.0],
            [0.5, 0.5],
        );
        assert_eq!(u32_at(&buf, 32), 0, "linear target");
        // z and w of the flags UVec4, then the struct tail.
        assert!(buf[40..48].iter().all(|b| *b == 0));
        assert!(buf[72..80].iter().all(|b| *b == 0));
    }

    #[test]
    fn tile_placement_maps_the_payload_through_the_src_to_dst_map() {
        // 10000x5000 at level 2 is 2500x1250. Tile (0, 0) is a full 512
        // square inside a 528-padded texture.
        let level = (2500, 1250);
        let key = TileKey {
            lod: 2,
            col: 0,
            row: 0,
        };
        let (tdst, tsrc) = tile_placement(
            [0.0, 0.0, 1.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
            level,
            key,
            (528, 528),
        );
        close(tdst[0], 0.0);
        close(tdst[1], 0.0);
        close(tdst[2], 512.0 / 2500.0);
        close(tdst[3], 512.0 / 1250.0);
        // The source rect insets past the 8 px gutter on every side.
        close(tsrc[0], 8.0 / 528.0);
        close(tsrc[1], 8.0 / 528.0);
        close(tsrc[2], 520.0 / 528.0);
        close(tsrc[3], 520.0 / 528.0);
    }

    #[test]
    fn a_clamped_edge_tile_reaches_the_placement_corner() {
        // The last tile of the 2500x1250 level is cut short to 452x226, so
        // its padded texture is 468x242 and its dst rect ends at the
        // placement's far corner.
        let level = (2500, 1250);
        let key = TileKey {
            lod: 2,
            col: 4,
            row: 2,
        };
        let dst = [0.1, 0.2, 0.9, 0.8];
        let (tdst, tsrc) = tile_placement(dst, [0.0, 0.0, 1.0, 1.0], level, key, (468, 242));
        close(tdst[0], 0.1 + 2048.0 / 2500.0 * 0.8);
        close(tdst[1], 0.2 + 1024.0 / 1250.0 * 0.6);
        close(tdst[2], 0.9);
        close(tdst[3], 0.8);
        close(tsrc[0], 8.0 / 468.0);
        close(tsrc[1], 8.0 / 242.0);
        close(tsrc[2], 460.0 / 468.0);
        close(tsrc[3], 234.0 / 242.0);
    }
}
