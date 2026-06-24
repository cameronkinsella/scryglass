//! Media decoding: format registry, cancellable load pipeline, and caches.
//!
//! Scryglass owns its decode path (rather than letting iced read files via
//! `Handle::from_path`) so that file reads are async (a stalled read
//! never blocks anything), stale loads are cancellable, EXIF orientation is
//! applied, and oversized images are downscaled before GPU upload.

pub mod animation;
pub mod archive;
pub mod cache;
pub mod decoders;
#[cfg(feature = "disk-thumbs")]
pub mod disk_thumbs;
#[cfg(not(feature = "disk-thumbs"))]
#[path = "disk_thumbs_stub.rs"]
pub mod disk_thumbs;
pub mod info;
pub mod pipeline;
pub mod registry;
pub mod thumbs;

/// Longest side of generated thumbnails, in pixels.
pub const THUMB_DIM: u32 = 256;

/// A decoded still image, ready for GPU upload.
pub struct DecodedImage {
    /// Pixel width after orientation and any downscale.
    pub width: u32,
    /// Pixel height after orientation and any downscale.
    pub height: u32,
    /// RGBA8 pixels with EXIF orientation already applied.
    pub pixels: Vec<u8>,
    /// Dimensions after orientation but before any downscale. The image's
    /// true size, used for zoom math and the footer.
    pub original_size: (u32, u32),
    /// Small preview generated from the decoded pixels, powering the
    /// filmstrip and instant placeholders. `None` when the image itself
    /// is already thumbnail-sized.
    pub thumbnail: Option<ThumbData>,
}

/// CPU-side thumbnail pixels (RGBA8, orientation applied).
#[derive(Debug, Clone)]
pub struct ThumbData {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    /// True dimensions of the image this previews.
    pub original_size: (u32, u32),
}

/// Decoded media of any kind.
pub enum DecodedMedia {
    Static(DecodedImage),
    /// A multi-frame animation (GIF, APNG, animated WebP), shared between
    /// the player's cache and active playback.
    Animated(std::sync::Arc<animation::AnimatedImage>),
}

/// Why a load produced no media.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MediaError {
    /// A newer navigation made this load irrelevant before it finished.
    #[error("load cancelled")]
    Cancelled,
    #[error("unsupported format")]
    Unsupported,
    #[error("could not read file: {0}")]
    Read(String),
    #[error("could not decode image: {0}")]
    Decode(String),
}

/// A file's true format, sniffed from its leading bytes. Used to warn when a
/// rename would give the file an extension that misrepresents its contents.
/// Only formats worth warning about are recognized. Anything else is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFormat {
    /// Human-facing label, e.g. "PNG".
    pub label: &'static str,
    /// Extensions that honestly name this format, e.g. `["jpg", "jpeg"]`.
    pub extensions: &'static [&'static str],
}

/// Identify a file's format from its first bytes, the same magic-byte signals
/// the decoder dispatch uses. `None` when it isn't a format we recognize.
pub fn sniff_format(magic: &[u8]) -> Option<FileFormat> {
    use image::ImageFormat as Kind;

    // SVG is XML text, so the binary signature table below misses it.
    if looks_like_svg(magic) {
        return Some(FileFormat {
            label: "SVG",
            extensions: &["svg", "svgz"],
        });
    }

    let kind = image::guess_format(magic).ok()?;
    let label = match kind {
        Kind::Png => "PNG",
        Kind::Jpeg => "JPEG",
        Kind::Gif => "GIF",
        Kind::WebP => "WebP",
        Kind::Tiff => "TIFF",
        Kind::Bmp => "BMP",
        Kind::Ico => "ICO",
        Kind::Avif => "AVIF",
        Kind::Qoi => "QOI",
        _ => return None,
    };
    Some(FileFormat {
        label,
        extensions: kind.extensions_str(),
    })
}

/// Whether the bytes begin an SVG document, allowing a BOM and leading space.
fn looks_like_svg(magic: &[u8]) -> bool {
    let trimmed = magic
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(magic)
        .trim_ascii_start();
    trimmed.starts_with(b"<svg") || trimmed.starts_with(b"<?xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn sniffs_png_by_content() {
        let format = sniff_format(&png_bytes()).expect("png should sniff");
        assert_eq!(format.label, "PNG");
        assert!(format.extensions.contains(&"png"));
    }

    #[test]
    fn sniffs_svg_text() {
        assert_eq!(sniff_format(b"<svg xmlns=").map(|f| f.label), Some("SVG"));
        assert_eq!(
            sniff_format(b"  <?xml version=").map(|f| f.label),
            Some("SVG")
        );
    }

    #[test]
    fn unrecognized_bytes_are_none() {
        assert!(sniff_format(b"not an image at all").is_none());
        assert!(sniff_format(&[]).is_none());
    }
}
