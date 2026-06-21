//! Decoded-frame data and the CPU YUV-to-RGBA path. Pure: no FFmpeg, no
//! GPU, so it all unit-tests.

use std::time::Duration;

/// YUV-to-RGB matrix the GPU converter should use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

/// YUV sample range (limited 16-235 or full 0-255).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvRange {
    Limited,
    Full,
}

/// Plane layout of a decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvFormat {
    /// Three planes: Y, U, V (software decode).
    I420,
    /// Two planes: Y plus interleaved UV (hardware download).
    Nv12,
}

/// A decoded frame in planar YUV 4:2:0, ready for the GPU converter.
/// Planes are tightly packed, with stride padding removed.
pub struct VideoFrame {
    /// Monotonic id, so the GPU side can skip re-uploading the same frame.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub format: YuvFormat,
    pub y: Vec<u8>,
    /// I420: the U plane. NV12: the interleaved UV plane (2 bytes/sample).
    pub u: Vec<u8>,
    /// I420: the V plane. NV12: empty.
    pub v: Vec<u8>,
    pub matrix: YuvMatrix,
    pub range: YuvRange,
    /// Presentation time relative to the session start (the seek point).
    pub timestamp: Duration,
}

impl VideoFrame {
    /// Convert this frame to RGBA8 on the CPU, for a one-off clipboard
    /// copy. Mirrors the GPU shader's matrix and range (chroma upsampled
    /// nearest, which is plenty for a still grab).
    pub fn to_rgba(&self) -> (u32, u32, Vec<u8>) {
        let (w, h, cw) = (
            self.width as usize,
            self.height as usize,
            self.chroma_width as usize,
        );
        let full = self.range == YuvRange::Full;
        let bt709 = self.matrix == YuvMatrix::Bt709;
        let nv12 = self.format == YuvFormat::Nv12;
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            let yrow = y * w;
            for x in 0..w {
                let yn = self.y[yrow + x] as f32 / 255.0;
                let (un, vn) = if nv12 {
                    let i = (y / 2) * cw * 2 + (x / 2) * 2;
                    (self.u[i] as f32 / 255.0, self.u[i + 1] as f32 / 255.0)
                } else {
                    let i = (y / 2) * cw + x / 2;
                    (self.u[i] as f32 / 255.0, self.v[i] as f32 / 255.0)
                };
                let (luma, cb, cr) = if full {
                    (yn, un - 0.5, vn - 0.5)
                } else {
                    (
                        (yn - 16.0 / 255.0) * (255.0 / 219.0),
                        (un - 128.0 / 255.0) * (255.0 / 224.0),
                        (vn - 128.0 / 255.0) * (255.0 / 224.0),
                    )
                };
                let (r, g, b) = if bt709 {
                    (
                        luma + 1.5748 * cr,
                        luma - 0.1873 * cb - 0.4681 * cr,
                        luma + 1.8556 * cb,
                    )
                } else {
                    (
                        luma + 1.402 * cr,
                        luma - 0.344136 * cb - 0.714136 * cr,
                        luma + 1.772 * cb,
                    )
                };
                let o = (yrow + x) * 4;
                out[o] = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
                out[o + 1] = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
                out[o + 2] = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
                out[o + 3] = 255;
            }
        }
        (self.width, self.height, out)
    }
}

/// Copy one plane into a tightly-packed buffer, dropping stride padding.
pub(crate) fn copy_plane(data: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let offset = row * stride;
        out.extend_from_slice(&data[offset..offset + width]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        format: YuvFormat,
        range: YuvRange,
        matrix: YuvMatrix,
        dims: (u32, u32, u32, u32),
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    ) -> VideoFrame {
        let (width, height, chroma_width, chroma_height) = dims;
        VideoFrame {
            id: 0,
            width,
            height,
            chroma_width,
            chroma_height,
            format,
            y,
            u,
            v,
            matrix,
            range,
            timestamp: Duration::ZERO,
        }
    }

    /// A 2x2 frame has 1x1 chroma; every luma sample shares the one chroma.
    fn gray_dims() -> (u32, u32, u32, u32) {
        (2, 2, 1, 1)
    }

    fn pixels(rgba: &[u8]) -> Vec<(u8, u8, u8, u8)> {
        rgba.chunks_exact(4)
            .map(|p| (p[0], p[1], p[2], p[3]))
            .collect()
    }

    #[test]
    fn output_is_width_times_height_rgba() {
        let f = frame(
            YuvFormat::I420,
            YuvRange::Full,
            YuvMatrix::Bt601,
            (4, 3, 2, 2),
            vec![128; 12],
            vec![128; 4],
            vec![128; 4],
        );
        let (w, h, out) = f.to_rgba();
        assert_eq!((w, h), (4, 3));
        assert_eq!(out.len(), 4 * 3 * 4);
    }

    // Full-range neutral chroma is 127.5; the nearest 8-bit value (128)
    // leaves a half-step tint, so a luma-only frame lands within one step
    // of gray rather than exactly gray.
    #[test]
    fn full_range_neutral_chroma_renders_near_gray() {
        for luma in [0u8, 128, 255] {
            let f = frame(
                YuvFormat::I420,
                YuvRange::Full,
                YuvMatrix::Bt601,
                gray_dims(),
                vec![luma; 4],
                vec![128],
                vec![128],
            );
            for (r, g, b, a) in pixels(&f.to_rgba().2) {
                assert_eq!(a, 255);
                for c in [r, g, b] {
                    assert!(c.abs_diff(luma) <= 1, "channel {c} far from {luma}");
                }
            }
        }
    }

    #[test]
    fn limited_range_maps_16_to_black_235_to_white() {
        let black = frame(
            YuvFormat::I420,
            YuvRange::Limited,
            YuvMatrix::Bt601,
            gray_dims(),
            vec![16; 4],
            vec![128],
            vec![128],
        );
        for px in pixels(&black.to_rgba().2) {
            assert_eq!(px, (0, 0, 0, 255));
        }
        let white = frame(
            YuvFormat::I420,
            YuvRange::Limited,
            YuvMatrix::Bt601,
            gray_dims(),
            vec![235; 4],
            vec![128],
            vec![128],
        );
        for px in pixels(&white.to_rgba().2) {
            assert_eq!(px, (255, 255, 255, 255));
        }
    }

    #[test]
    fn nv12_reads_same_chroma_as_i420() {
        let dims = gray_dims();
        let i420 = frame(
            YuvFormat::I420,
            YuvRange::Full,
            YuvMatrix::Bt601,
            dims,
            vec![128; 4],
            vec![100],
            vec![200],
        );
        let nv12 = frame(
            YuvFormat::Nv12,
            YuvRange::Full,
            YuvMatrix::Bt601,
            dims,
            vec![128; 4],
            vec![100, 200],
            vec![],
        );
        assert_eq!(i420.to_rgba().2, nv12.to_rgba().2);
    }

    #[test]
    fn matrix_choice_changes_colored_output() {
        let dims = gray_dims();
        let bt601 = frame(
            YuvFormat::I420,
            YuvRange::Full,
            YuvMatrix::Bt601,
            dims,
            vec![128; 4],
            vec![100],
            vec![200],
        );
        let bt709 = frame(
            YuvFormat::I420,
            YuvRange::Full,
            YuvMatrix::Bt709,
            dims,
            vec![128; 4],
            vec![100],
            vec![200],
        );
        assert_ne!(bt601.to_rgba().2, bt709.to_rgba().2);
    }

    #[test]
    fn copy_plane_without_padding_is_identity() {
        let data = vec![1, 2, 3, 4, 5, 6];
        assert_eq!(copy_plane(&data, 3, 3, 2), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn copy_plane_strips_row_padding() {
        // stride 4, width 2: two trailing pad bytes per row are dropped.
        let data = vec![1, 2, 0, 0, 3, 4, 0, 0];
        assert_eq!(copy_plane(&data, 4, 2, 2), vec![1, 2, 3, 4]);
    }

    #[test]
    fn copy_plane_single_row() {
        let data = vec![9, 9, 9, 7, 7];
        assert_eq!(copy_plane(&data, 5, 3, 1), vec![9, 9, 9]);
    }
}
