//! Format registry: dispatches files to decoders by magic bytes, with
//! file extension as the fallback. Magic bytes win so mislabeled files
//! (a JPEG renamed to .png) still open.

use std::path::Path;
use std::sync::LazyLock;

use super::{DecodedMedia, MediaError};

/// Conservative wgpu texture-size limit. iced doesn't expose the real
/// device limit, but 8192 is the downlevel default and safe everywhere.
pub const MAX_TEXTURE_DIM: u32 = 8192;

/// Knobs for a single decode.
#[derive(Debug, Clone, Copy)]
pub struct DecodeOpts {
    /// Hard cap for either dimension. Larger images are downscaled to fit
    /// within GPU texture limits. The original size is preserved in
    /// [`super::DecodedImage::original_size`].
    pub max_dimension: u32,
}

impl Default for DecodeOpts {
    fn default() -> Self {
        Self {
            max_dimension: MAX_TEXTURE_DIM,
        }
    }
}

/// A pluggable image format decoder.
pub trait ImageFormat: Send + Sync {
    /// Lowercase extensions (no dot) this format claims.
    fn extensions(&self) -> &'static [&'static str];
    /// Whether the first bytes of a file look like this format.
    fn sniff(&self, magic: &[u8]) -> bool;
    /// Decode a full file into media.
    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError>;
    /// Claim this format's extensions BEFORE magic sniffing. For container
    /// formats whose magic collides with another format (RAW files are
    /// TIFF-structured), the explicit extension is the stronger signal.
    fn prefer_extension(&self) -> bool {
        false
    }
}

/// The set of known formats.
pub struct Registry {
    formats: Vec<Box<dyn ImageFormat>>,
}

impl Registry {
    fn new() -> Self {
        let mut formats: Vec<Box<dyn ImageFormat>> =
            vec![Box::new(super::decoders::image_rs::ImageRs)];
        #[cfg(feature = "jxl")]
        formats.push(Box::new(super::decoders::jxl::Jxl));
        #[cfg(feature = "svg")]
        formats.push(Box::new(super::decoders::svg::Svg));
        #[cfg(feature = "raw")]
        formats.push(Box::new(super::decoders::raw::Raw));
        #[cfg(feature = "heif")]
        formats.push(Box::new(super::decoders::heif::Heif));
        #[cfg(feature = "video")]
        formats.push(Box::new(super::decoders::avif::Avif));
        Self { formats }
    }

    /// All extensions claimed by registered formats. The single source of
    /// truth for directory scans and file dialog filters.
    pub fn extensions(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.formats
            .iter()
            .flat_map(|f| f.extensions().iter().copied())
    }

    /// Find the decoder for a file: extension-priority formats first, then
    /// magic bytes, then plain extension fallback.
    pub fn find(&self, path: &Path, magic: &[u8]) -> Option<&dyn ImageFormat> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());

        if let Some(ext) = &ext
            && let Some(f) = self
                .formats
                .iter()
                .find(|f| f.prefer_extension() && f.extensions().contains(&ext.as_str()))
        {
            return Some(f.as_ref());
        }
        if let Some(f) = self.formats.iter().find(|f| f.sniff(magic)) {
            return Some(f.as_ref());
        }
        let ext = ext?;
        self.formats
            .iter()
            .find(|f| f.extensions().contains(&ext.as_str()))
            .map(|f| f.as_ref())
    }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// The process-wide format registry.
pub fn global() -> &'static Registry {
    &REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A tiny valid PNG, encoded in-memory.
    fn png_bytes() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([255, 0, 0, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn find_by_magic_bytes() {
        let bytes = png_bytes();
        let found = global().find(Path::new("whatever.bin"), &bytes[..16]);
        assert!(
            found.is_some(),
            "PNG magic should match regardless of extension"
        );
    }

    #[test]
    fn mislabeled_extension_still_resolves_by_magic() {
        let bytes = png_bytes();
        let found = global().find(Path::new("actually_a_png.jpg"), &bytes[..16]);
        assert!(found.is_some());
        // And the decode succeeds despite the lying extension.
        let media = found.unwrap().decode(&bytes, &DecodeOpts::default());
        assert!(media.is_ok());
    }

    #[test]
    fn unknown_magic_falls_back_to_extension() {
        let garbage = [0u8; 16];
        let found = global().find(Path::new("photo.png"), &garbage);
        assert!(found.is_some(), "extension fallback should match");
    }

    #[test]
    fn unknown_magic_and_extension_is_none() {
        let garbage = [0u8; 16];
        assert!(global().find(Path::new("file.xyz"), &garbage).is_none());
        assert!(global().find(Path::new("no_extension"), &garbage).is_none());
    }
}
