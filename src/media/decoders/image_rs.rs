//! Decoder for the formats handled by the `image` crate:
//! PNG, JPEG, BMP, WebP, TIFF, ICO, and AVIF.
//!
//! EXIF orientation is read from the decoder and applied to the pixels, so
//! portrait photos display upright. Images larger than the GPU texture cap
//! are downscaled (the original dimensions are preserved for zoom math).

use std::io::Cursor;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};

use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedImage, DecodedMedia, MediaError};

pub struct ImageRs;

impl ImageFormat for ImageRs {
    fn name(&self) -> &'static str {
        "image-rs"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[
            "png", "jpg", "jpeg", "bmp", "webp", "tiff", "tif", "ico", "avif",
        ]
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        sniff(magic)
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| MediaError::Read(e.to_string()))?;
        let mut decoder = reader
            .into_decoder()
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let mut img =
            DynamicImage::from_decoder(decoder).map_err(|e| MediaError::Decode(e.to_string()))?;
        img.apply_orientation(orientation);
        Ok(DecodedMedia::Static(finish(img, opts)))
    }
}

/// Cap dimensions to the texture limit and convert to RGBA8.
fn finish(img: DynamicImage, opts: &DecodeOpts) -> DecodedImage {
    let original_size = (img.width(), img.height());

    let img = if img.width().max(img.height()) > opts.max_dimension {
        img.resize(opts.max_dimension, opts.max_dimension, FilterType::Triangle)
    } else {
        img
    };

    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    DecodedImage {
        width,
        height,
        pixels: rgba.into_raw(),
        original_size,
    }
}

/// Recognize the magic bytes of the formats this decoder handles.
fn sniff(magic: &[u8]) -> bool {
    if magic.len() < 12 {
        return false;
    }
    let png = magic.starts_with(&[0x89, b'P', b'N', b'G']);
    let jpeg = magic.starts_with(&[0xFF, 0xD8, 0xFF]);
    let bmp = magic.starts_with(b"BM");
    let webp = magic.starts_with(b"RIFF") && &magic[8..12] == b"WEBP";
    let tiff = magic.starts_with(&[0x49, 0x49, 0x2A, 0x00])
        || magic.starts_with(&[0x4D, 0x4D, 0x00, 0x2A]);
    let ico = magic.starts_with(&[0x00, 0x00, 0x01, 0x00]);
    let avif = &magic[4..8] == b"ftyp" && &magic[8..12] == b"avif";
    png || jpeg || bmp || webp || tiff || ico || avif
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8], opts: &DecodeOpts) -> DecodedImage {
        match ImageRs.decode(bytes, opts).unwrap() {
            DecodedMedia::Static(img) => img,
        }
    }

    fn encode_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 128, 255, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// A JPEG with an EXIF APP1 segment declaring the given orientation,
    /// built by splicing a handcrafted TIFF blob after the SOI marker.
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        let jpeg = out.into_inner();

        // EXIF payload: "Exif\0\0" + little-endian TIFF header + IFD0 with a
        // single Orientation (0x0112) SHORT entry.
        let mut exif: Vec<u8> = Vec::new();
        exif.extend_from_slice(b"Exif\0\0");
        exif.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]); // II, magic 42
        exif.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
        exif.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        exif.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
        exif.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
        exif.extend_from_slice(&1u32.to_le_bytes()); // count
        exif.extend_from_slice(&orientation.to_le_bytes());
        exif.extend_from_slice(&0u16.to_le_bytes()); // value padding
        exif.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        let mut app1: Vec<u8> = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);

        // Splice APP1 right after SOI (first two bytes).
        let mut spliced = jpeg[..2].to_vec();
        spliced.extend_from_slice(&app1);
        spliced.extend_from_slice(&jpeg[2..]);
        spliced
    }

    #[test]
    fn decodes_png_with_correct_dimensions() {
        let img = decode(&encode_png(4, 2), &DecodeOpts::default());
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(img.original_size, (4, 2));
        assert_eq!(img.pixels.len(), 4 * 2 * 4);
        assert_eq!(&img.pixels[..4], &[0, 128, 255, 255]);
    }

    #[test]
    fn applies_exif_orientation_rotate_90() {
        // Orientation 6 = rotate 90° CW: a 4×2 file displays as 2×4.
        let img = decode(&jpeg_with_orientation(4, 2, 6), &DecodeOpts::default());
        assert_eq!((img.width, img.height), (2, 4));
        assert_eq!(img.original_size, (2, 4));
    }

    #[test]
    fn no_orientation_leaves_dimensions_alone() {
        let img = decode(&jpeg_with_orientation(4, 2, 1), &DecodeOpts::default());
        assert_eq!((img.width, img.height), (4, 2));
    }

    #[test]
    fn oversized_image_downscales_to_cap() {
        let opts = DecodeOpts { max_dimension: 64 };
        let img = decode(&encode_png(100, 50), &opts);
        assert_eq!((img.width, img.height), (64, 32));
        assert_eq!(img.original_size, (100, 50));
    }

    #[test]
    fn sniffs_common_formats() {
        assert!(sniff(&encode_png(1, 1)[..16]));
        let jpeg = jpeg_with_orientation(1, 1, 1);
        assert!(sniff(&jpeg[..16]));
        assert!(!sniff(&[0u8; 16]));
        assert!(!sniff(b"short"));
    }

    #[test]
    fn corrupt_data_is_a_decode_error() {
        let mut bytes = encode_png(4, 4);
        bytes.truncate(20); // valid magic, broken body
        assert!(matches!(
            ImageRs.decode(&bytes, &DecodeOpts::default()),
            Err(MediaError::Decode(_)) | Err(MediaError::Read(_))
        ));
    }
}
