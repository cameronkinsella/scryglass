//! Format decoder implementations.

pub mod image_rs;

#[cfg(feature = "video")]
pub mod avif;
#[cfg(feature = "heif")]
pub mod heif;
#[cfg(feature = "jxl")]
pub mod jxl;
#[cfg(feature = "raw")]
pub mod raw;
#[cfg(feature = "svg")]
pub mod svg;

use image::{DynamicImage, imageops::FilterType};

use crate::media::DecodedImage;
use crate::media::regime;
use crate::media::registry::DecodeOpts;

/// Shared decode tail: reduce an oversized decode to the texture limit and
/// the RAM budget, derive a thumbnail, and convert to RGBA8.
// TODO: the budget bounds what stays resident, not the decode: the full-size
// image exists in RAM transiently before this clamp. Bounding the peak too
// needs format-aware scaled decoding (JPEG IDCT scaling and friends).
pub(crate) fn finish(img: DynamicImage, opts: &DecodeOpts) -> DecodedImage {
    let original_size = (img.width(), img.height());

    let target = if opts.tile_capable {
        regime::decode_target(original_size, opts.max_dimension, opts.ram_budget)
    } else {
        regime::fit_within(original_size, opts.max_dimension)
    };
    let img = match target {
        // Full variable-width support, so any reduction ratio stays alias-free.
        Some((w, h)) => img.resize_exact(w, h, FilterType::CatmullRom),
        None => img,
    };

    // Always produce a thumbnail, since the pixels are already decoded.
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    let thumbnail = if width.max(height) > crate::media::THUMB_DIM {
        let t = image::DynamicImage::ImageRgba8(rgba.clone())
            .thumbnail(crate::media::THUMB_DIM, crate::media::THUMB_DIM)
            .into_rgba8();
        let (tw, th) = t.dimensions();
        crate::media::ThumbData {
            width: tw,
            height: th,
            pixels: t.into_raw(),
            original_size,
        }
    } else {
        crate::media::ThumbData {
            width,
            height,
            pixels: rgba.as_raw().clone(),
            original_size,
        }
    };

    DecodedImage {
        width,
        height,
        pixels: rgba.into_raw(),
        original_size,
        thumbnail: Some(thumbnail),
    }
}
