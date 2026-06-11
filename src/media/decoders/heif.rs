//! HEIC/HEIF decoding via `libheif-rs` (cargo feature `heif`).
//!
//! Requires the system libheif C library at build and run time, so this
//! stays opt-in and off the default feature set.

use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

use super::finish;
use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedMedia, MediaError};

pub struct Heif;

impl ImageFormat for Heif {
    fn extensions(&self) -> &'static [&'static str] {
        &["heic", "heif"]
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        sniff(magic)
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        let ctx =
            HeifContext::read_from_bytes(bytes).map_err(|e| MediaError::Decode(e.to_string()))?;
        let handle = ctx
            .primary_image_handle()
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        let decoded = LibHeif::new()
            .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
            .map_err(|e| MediaError::Decode(e.to_string()))?;

        let planes = decoded.planes();
        let plane = planes
            .interleaved
            .ok_or_else(|| MediaError::Decode("heif: no interleaved plane".into()))?;
        let (width, height) = (plane.width, plane.height);
        let stride = plane.stride;

        // Rows can carry stride padding, copy row by row.
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height as usize {
            let start = row * stride;
            rgba.extend_from_slice(&plane.data[start..start + (width as usize) * 4]);
        }

        let buffer = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| MediaError::Decode("heif buffer size mismatch".into()))?;
        Ok(DecodedMedia::Static(finish(
            image::DynamicImage::ImageRgba8(buffer),
            opts,
        )))
    }
}

/// ISOBMFF `ftyp` brands used by HEIF still images.
fn sniff(magic: &[u8]) -> bool {
    magic.len() >= 12
        && &magic[4..8] == b"ftyp"
        && matches!(
            &magic[8..12],
            b"heic" | b"heix" | b"hevc" | b"mif1" | b"msf1"
        )
}
