//! Fast thumbnail extraction from a file prefix.
//!
//! Camera JPEGs embed a small preview (EXIF IFD1) within the first few
//! kilobytes of the file. Reading just a prefix and decoding that preview
//! takes milliseconds even over slow storage, fast enough to show a blurred
//! placeholder for every image while scrubbing, long before the full
//! multi-megabyte decode completes.

use std::io::Cursor;

use exif::{In, Tag};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};

use super::{THUMB_DIM, ThumbData};

/// How much of the file to read for thumbnail probing. EXIF lives at the
/// start, and 256 KiB comfortably covers the APP1 segment and the SOF header.
pub const PREFIX_LEN: usize = 256 * 1024;

/// Extract and decode the embedded EXIF thumbnail from a file prefix.
///
/// Returns `None` when there's no usable embedded thumbnail (common for
/// PNGs and screenshots) or when the image's true dimensions can't be
/// determined from the prefix.
pub fn thumb_from_prefix(prefix: &[u8]) -> Option<ThumbData> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(prefix))
        .ok()?;

    // Embedded thumbnail bytes: IFD1's JPEGInterchangeFormat points into
    // the raw TIFF buffer.
    let offset = exif
        .get_field(Tag::JPEGInterchangeFormat, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let len = exif
        .get_field(Tag::JPEGInterchangeFormatLength, In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let jpeg = exif.buf().get(offset..offset + len)?;

    // The embedded preview is stored unrotated, like the main image,
    // apply the same orientation so the placeholder matches.
    let orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .and_then(|v| Orientation::from_exif(v as u8))
        .unwrap_or(Orientation::NoTransforms);

    let mut thumb = ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    thumb.apply_orientation(orientation);

    // True image dimensions from the header, orientation applied to match
    // what the full decode will report.
    let (w, h) = probe_dimensions(prefix)?;
    let original_size = if orientation_swaps_axes(orientation) {
        (h, w)
    } else {
        (w, h)
    };

    let rgba = thumb.into_rgba8();
    let (tw, th) = rgba.dimensions();
    Some(ThumbData {
        width: tw,
        height: th,
        pixels: rgba.into_raw(),
        original_size,
    })
}

/// Decode a whole file and downscale it to a thumbnail, the fallback for
/// images without an embedded EXIF preview (PNGs, screenshots, GIFs).
/// Much heavier than the prefix probe, reserved for background work.
pub fn thumb_from_bytes(bytes: &[u8]) -> Option<ThumbData> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).ok()?;
    img.apply_orientation(orientation);

    let original_size = (img.width(), img.height());
    let thumb = img.thumbnail(THUMB_DIM, THUMB_DIM).into_rgba8();
    let (width, height) = thumb.dimensions();
    Some(ThumbData {
        width,
        height,
        pixels: thumb.into_raw(),
        original_size,
    })
}

/// Image dimensions as stored in the header (pre-orientation).
fn probe_dimensions(prefix: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(prefix))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn orientation_swaps_axes(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_jpeg(width: u32, height: u32, shade: u8) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([shade, shade, shade]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// A JPEG whose EXIF declares `orientation` and embeds `thumb_jpeg`
    /// as the IFD1 thumbnail.
    fn jpeg_with_thumbnail(
        width: u32,
        height: u32,
        orientation: u16,
        thumb_jpeg: &[u8],
    ) -> Vec<u8> {
        let main = encode_jpeg(width, height, 200);

        // TIFF layout (little endian), offsets from the TIFF header:
        //   0: header (8 bytes)
        //   8: IFD0: 1 entry (orientation) + pointer to IFD1
        //  26: IFD1: 2 entries (thumb offset/length), no next IFD
        //  56: thumbnail JPEG bytes
        let thumb_offset: u32 = 56;
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
        tiff.extend_from_slice(&8u32.to_le_bytes());
        // IFD0
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes()); // padding
        tiff.extend_from_slice(&26u32.to_le_bytes()); // next IFD → IFD1
        // IFD1
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0x0201u16.to_le_bytes()); // JPEGInterchangeFormat
        tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&thumb_offset.to_le_bytes());
        tiff.extend_from_slice(&0x0202u16.to_le_bytes()); // ...FormatLength
        tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&(thumb_jpeg.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        assert_eq!(tiff.len(), thumb_offset as usize);
        tiff.extend_from_slice(thumb_jpeg);

        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff);

        let mut app1: Vec<u8> = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&exif);

        let mut spliced = main[..2].to_vec();
        spliced.extend_from_slice(&app1);
        spliced.extend_from_slice(&main[2..]);
        spliced
    }

    #[test]
    fn extracts_embedded_thumbnail() {
        let thumb_jpeg = encode_jpeg(8, 4, 50);
        let file = jpeg_with_thumbnail(640, 320, 1, &thumb_jpeg);

        let thumb = thumb_from_prefix(&file).expect("thumbnail should extract");
        assert_eq!((thumb.width, thumb.height), (8, 4));
        assert_eq!(thumb.original_size, (640, 320));
    }

    #[test]
    fn applies_orientation_to_thumb_and_dimensions() {
        let thumb_jpeg = encode_jpeg(8, 4, 50);
        // Orientation 6 = rotate 90° CW.
        let file = jpeg_with_thumbnail(640, 320, 6, &thumb_jpeg);

        let thumb = thumb_from_prefix(&file).expect("thumbnail should extract");
        assert_eq!((thumb.width, thumb.height), (4, 8));
        assert_eq!(thumb.original_size, (320, 640));
    }

    #[test]
    fn works_on_a_truncated_prefix() {
        let thumb_jpeg = encode_jpeg(8, 4, 50);
        let file = jpeg_with_thumbnail(640, 320, 1, &thumb_jpeg);
        // Everything needed lives at the front of the file.
        let prefix = &file[..file.len().min(8 * 1024)];

        let thumb = thumb_from_prefix(prefix).expect("prefix should be enough");
        assert_eq!(thumb.original_size, (640, 320));
    }

    #[test]
    fn png_without_exif_returns_none() {
        let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        assert!(thumb_from_prefix(&out.into_inner()).is_none());
    }

    #[test]
    fn jpeg_without_thumbnail_returns_none() {
        // Has EXIF (orientation only) but no IFD1 thumbnail.
        let plain = encode_jpeg(64, 32, 99);
        assert!(thumb_from_prefix(&plain).is_none());
    }

    #[test]
    fn thumb_from_bytes_decodes_and_downscales_png() {
        let img = image::RgbaImage::from_pixel(600, 300, image::Rgba([7, 8, 9, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();

        let thumb = thumb_from_bytes(&out.into_inner()).expect("png should decode");
        assert_eq!(thumb.original_size, (600, 300));
        assert_eq!((thumb.width, thumb.height), (THUMB_DIM, THUMB_DIM / 2));
    }

    #[test]
    fn thumb_from_bytes_decodes_gif_first_frame() {
        let img = image::RgbaImage::from_pixel(40, 20, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Gif).unwrap();

        let thumb = thumb_from_bytes(&out.into_inner()).expect("gif should decode");
        assert_eq!(thumb.original_size, (40, 20));
    }

    #[test]
    fn thumb_from_bytes_rejects_garbage() {
        assert!(thumb_from_bytes(b"definitely not an image").is_none());
    }
}
