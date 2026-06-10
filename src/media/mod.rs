//! Media decoding: format registry, cancellable load pipeline, and caches.
//!
//! Scryglass owns its decode path (rather than letting iced read files via
//! `Handle::from_path`) so that file reads are async (a stalled read
//! never blocks anything), stale loads are cancellable, EXIF orientation is
//! applied, and oversized images are downscaled before GPU upload.

pub mod cache;
pub mod decoders;
pub mod pipeline;
pub mod registry;

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
