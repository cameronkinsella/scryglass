//! AVIF decoding through FFmpeg (cargo feature `video`).
//!
//! An AVIF still is an AV1 keyframe in a HEIF container, and the bundled
//! FFmpeg already decodes AV1 for video, so stills ride the same code
//! instead of pulling in a second AV1 decoder.

use std::sync::atomic::{AtomicU64, Ordering};

use super::finish;
use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedMedia, MediaError};

pub struct Avif;

impl ImageFormat for Avif {
    fn extensions(&self) -> &'static [&'static str] {
        &["avif"]
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        sniff(magic)
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        // FFmpeg's demuxer wants a real file. Stills are small, and the
        // extraction dir is swept at startup, so strays cannot pile up.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = crate::video::extraction_dir();
        std::fs::create_dir_all(&dir).map_err(|e| MediaError::Read(e.to_string()))?;
        let path = dir.join(format!(
            "avif-{}-{}.avif",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).map_err(|e| MediaError::Read(e.to_string()))?;

        let frame = crate::video::first_frame(&path, None);
        let _ = std::fs::remove_file(&path);
        let frame = frame.ok_or_else(|| MediaError::Decode("ffmpeg rejected AVIF".into()))?;

        let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
            .ok_or_else(|| MediaError::Decode("bad frame dimensions".into()))?;
        Ok(DecodedMedia::Static(finish(
            image::DynamicImage::ImageRgba8(img),
            opts,
        )))
    }
}

/// Recognize the HEIF brands AVIF files carry.
fn sniff(magic: &[u8]) -> bool {
    magic.len() >= 12 && &magic[4..8] == b"ftyp" && matches!(&magic[8..12], b"avif" | b"avis")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a real file when one is supplied, since AVIF encoders are
    /// deliberately not part of the dependency tree.
    #[test]
    fn decodes_real_avif_when_provided() {
        let Ok(path) = std::env::var("SCRY_AVIF_FIXTURE") else {
            eprintln!("skipping: set SCRY_AVIF_FIXTURE to an .avif file");
            return;
        };
        let bytes = std::fs::read(path).unwrap();
        assert!(sniff(&bytes[..12]));
        let DecodedMedia::Static(img) = Avif.decode(&bytes, &DecodeOpts::default()).unwrap() else {
            panic!("expected static media");
        };
        assert!(img.width > 0 && img.height > 0);
        assert_eq!(img.pixels.len(), (img.width * img.height * 4) as usize);
    }

    #[test]
    fn sniffs_avif_brands() {
        let mut header = vec![0, 0, 0, 32];
        header.extend_from_slice(b"ftypavif");
        header.extend_from_slice(&[0; 8]);
        assert!(sniff(&header));

        let mut animated = vec![0, 0, 0, 32];
        animated.extend_from_slice(b"ftypavis");
        animated.extend_from_slice(&[0; 8]);
        assert!(sniff(&animated));

        // HEIC is a different decoder.
        let mut heic = vec![0, 0, 0, 32];
        heic.extend_from_slice(b"ftypheic");
        heic.extend_from_slice(&[0; 8]);
        assert!(!sniff(&heic));
    }
}
