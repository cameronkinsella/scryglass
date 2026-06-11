//! SVG rendering via `resvg`. Vectors have no native pixel size, so they're
//! rasterized at the decode cap (so thumbnails render small and cheap),
//! and `original_size` is the rendered size, so 100% zoom shows the raster
//! as produced.

use std::sync::{Arc, LazyLock};

use resvg::{tiny_skia, usvg};

use super::finish;
use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedMedia, MediaError};

/// Rasterizing beyond this is wasted work even when the texture cap is
/// higher. SVGs stay crisp through the GPU upscale at viewing sizes.
const MAX_RASTER: u32 = 2048;

/// System fonts load once (text elements need them), costs ~100ms on first use.
static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

pub struct Svg;

impl ImageFormat for Svg {
    fn extensions(&self) -> &'static [&'static str] {
        &["svg"]
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        sniff(magic)
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        let tree_opts = usvg::Options {
            fontdb: FONTS.clone(),
            ..Default::default()
        };
        let tree = usvg::Tree::from_data(bytes, &tree_opts)
            .map_err(|e| MediaError::Decode(e.to_string()))?;

        let size = tree.size();
        if size.width() <= 0.0 || size.height() <= 0.0 {
            return Err(MediaError::Decode("svg has no size".into()));
        }
        let target = opts.max_dimension.min(MAX_RASTER) as f32;
        let scale = target / size.width().max(size.height());
        let width = ((size.width() * scale).round() as u32).max(1);
        let height = ((size.height() * scale).round() as u32).max(1);

        let mut pixmap = tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| MediaError::Decode("svg raster too large".into()))?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        // tiny-skia pixels are premultiplied but the GPU wants straight RGBA.
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for px in pixmap.pixels() {
            let c = px.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }

        let buffer = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| MediaError::Decode("svg buffer size mismatch".into()))?;
        Ok(DecodedMedia::Static(finish(
            image::DynamicImage::ImageRgba8(buffer),
            opts,
        )))
    }
}

/// SVGs are XML text: look for an `<svg` or `<?xml` start, tolerating a
/// UTF-8 BOM and leading whitespace.
fn sniff(magic: &[u8]) -> bool {
    let start = magic.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(magic);
    let trimmed = start
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(&b""[..], |i| &start[i..]);
    trimmed.starts_with(b"<svg") || trimmed.starts_with(b"<?xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED_SQUARE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="5"><rect width="10" height="5" fill="#ff0000"/></svg>"##;

    #[test]
    fn renders_svg_at_cap_preserving_aspect() {
        let opts = DecodeOpts { max_dimension: 100 };
        let DecodedMedia::Static(img) = Svg.decode(RED_SQUARE, &opts).unwrap();
        assert_eq!((img.width, img.height), (100, 50));
        assert_eq!(&img.pixels[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn sniffs_svg_text() {
        assert!(sniff(b"<svg xmlns="));
        assert!(sniff(b"<?xml version="));
        assert!(sniff(b"\xEF\xBB\xBF<svg"));
        assert!(sniff(b"  <svg"));
        assert!(!sniff(b"\x89PNG\r\n"));
    }

    #[test]
    fn invalid_svg_is_a_decode_error() {
        assert!(Svg.decode(b"<svg", &DecodeOpts::default()).is_err());
    }
}
