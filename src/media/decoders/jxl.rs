//! JPEG XL decoder via the pure-Rust `jxl-oxide` crate.

use std::io::Cursor;

use jxl_oxide::JxlImage;

use super::finish;
use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedMedia, MediaError};

pub struct Jxl;

impl ImageFormat for Jxl {
    fn extensions(&self) -> &'static [&'static str] {
        &["jxl"]
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        sniff(magic)
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        let image = JxlImage::builder()
            .read(Cursor::new(bytes))
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        let render = image
            .render_frame(0)
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        let frame = render.image_all_channels();
        let (width, height) = (frame.width() as u32, frame.height() as u32);
        let channels = frame.channels();
        let samples = frame.buf();

        // Samples are f32 in [0, 1], expand gray/rgb to RGBA8.
        let pixel_count = (width as usize) * (height as usize);
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        for i in 0..pixel_count {
            let s = &samples[i * channels..(i + 1) * channels];
            let (r, g, b, a) = match channels {
                1 => (s[0], s[0], s[0], 1.0),
                2 => (s[0], s[0], s[0], s[1]),
                3 => (s[0], s[1], s[2], 1.0),
                _ => (s[0], s[1], s[2], s[3]),
            };
            rgba.extend_from_slice(&[to_u8(r), to_u8(g), to_u8(b), to_u8(a)]);
        }

        let buffer = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| MediaError::Decode("jxl buffer size mismatch".into()))?;
        Ok(DecodedMedia::Static(finish(
            image::DynamicImage::ImageRgba8(buffer),
            opts,
        )))
    }
}

/// JXL bare codestream (FF 0A) or ISOBMFF container.
fn sniff(magic: &[u8]) -> bool {
    magic.starts_with(&[0xFF, 0x0A])
        || magic.starts_with(&[
            0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
        ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_jxl(width: u32, height: u32) -> Vec<u8> {
        use zune_core::bit_depth::BitDepth;
        use zune_core::colorspace::ColorSpace;
        use zune_core::options::EncoderOptions;
        let pixels: Vec<u8> = (0..width * height).flat_map(|_| [200u8, 100, 50]).collect();
        let opts = EncoderOptions::new(
            width as usize,
            height as usize,
            ColorSpace::RGB,
            BitDepth::Eight,
        );
        let mut out = Vec::new();
        zune_jpegxl::JxlSimpleEncoder::new(&pixels, opts)
            .encode(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn decodes_jxl_roundtrip() {
        let bytes = encode_jxl(6, 4);
        assert!(sniff(&bytes[..12.min(bytes.len())]));
        let DecodedMedia::Static(img) = Jxl.decode(&bytes, &DecodeOpts::default()).unwrap();
        assert_eq!((img.width, img.height), (6, 4));
        // Lossless modular encode: exact pixel match.
        assert_eq!(&img.pixels[..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn garbage_is_a_decode_error() {
        assert!(
            Jxl.decode(&[0xFF, 0x0A, 1, 2, 3], &DecodeOpts::default())
                .is_err()
        );
    }
}
