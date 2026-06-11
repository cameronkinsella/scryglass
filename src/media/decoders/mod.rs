//! Format decoder implementations.

pub mod image_rs;

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
use crate::media::registry::DecodeOpts;

/// Shared decode tail: cap dimensions to the texture limit, derive a
/// thumbnail, and convert to RGBA8.
pub(crate) fn finish(img: DynamicImage, opts: &DecodeOpts) -> DecodedImage {
    let original_size = (img.width(), img.height());

    let img = if img.width().max(img.height()) > opts.max_dimension {
        img.resize(opts.max_dimension, opts.max_dimension, FilterType::Triangle)
    } else {
        img
    };

    // Thumbnails come nearly free here: the pixels are already decoded.
    let thumbnail = if img.width().max(img.height()) > crate::media::THUMB_DIM {
        let t = img
            .thumbnail(crate::media::THUMB_DIM, crate::media::THUMB_DIM)
            .into_rgba8();
        let (tw, th) = t.dimensions();
        Some(crate::media::ThumbData {
            width: tw,
            height: th,
            pixels: t.into_raw(),
            original_size,
        })
    } else {
        None
    };

    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    DecodedImage {
        width,
        height,
        pixels: rgba.into_raw(),
        original_size,
        thumbnail,
    }
}
