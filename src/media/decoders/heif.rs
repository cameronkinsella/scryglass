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

#[cfg(test)]
mod tests {
    use super::*;
    use libheif_rs::{Channel, ColorSpace, CompressionFormat, EncoderQuality, RgbChroma};

    /// Encode a small HEIC in-memory through libheif's own HEVC encoder.
    fn encode_heic(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut image =
            libheif_rs::Image::new(width, height, ColorSpace::Rgb(RgbChroma::Rgba)).unwrap();
        image
            .create_plane(Channel::Interleaved, width, height, 32)
            .unwrap();
        let mut planes = image.planes_mut();
        let plane = planes.interleaved.as_mut().unwrap();
        let stride = plane.stride;
        for row in 0..height as usize {
            for col in 0..width as usize {
                let offset = row * stride + col * 4;
                plane.data[offset..offset + 4].copy_from_slice(&rgba);
            }
        }

        let lib = LibHeif::new();
        let mut encoder = lib.encoder_for_format(CompressionFormat::Hevc).unwrap();
        encoder.set_quality(EncoderQuality::LossLess).unwrap();
        let mut ctx = HeifContext::new().unwrap();
        ctx.encode_image(&image, &mut encoder, None).unwrap();
        ctx.write_to_bytes().unwrap()
    }

    #[test]
    fn decodes_heic_roundtrip() {
        let bytes = encode_heic(64, 48, [200, 100, 50, 255]);
        assert!(sniff(&bytes[..12]), "encoded HEIC should sniff as HEIF");

        let DecodedMedia::Static(img) = Heif.decode(&bytes, &DecodeOpts::default()).unwrap() else {
            panic!("expected static media");
        };
        assert_eq!((img.width, img.height), (64, 48));
        assert_eq!(img.original_size, (64, 48));
        // Lossless HEVC: exact pixel match.
        assert_eq!(&img.pixels[..4], &[200, 100, 50, 255]);

        // Optional fixture export for manual checks in the running app.
        if let Ok(path) = std::env::var("SCRY_KEEP_HEIC") {
            std::fs::write(path, &bytes).unwrap();
        }
    }

    #[test]
    fn garbage_is_a_decode_error() {
        assert!(Heif.decode(&[0u8; 32], &DecodeOpts::default()).is_err());
    }
}
