//! Media decoding: format registry, cancellable load pipeline, and caches.
//!
//! Scryglass owns its decode path (rather than letting iced read files via
//! `Handle::from_path`) so that file reads are async (a stalled read
//! never blocks anything), stale loads are cancellable, EXIF orientation is
//! applied, and oversized images are downscaled before GPU upload.

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
