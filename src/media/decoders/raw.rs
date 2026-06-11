//! Camera RAW support via `rawler`. It extracts the embedded JPEG preview
//! (no demosaic, full RAW development is a non-goal).
//!
//! RAW containers are TIFF-structured, so their magic bytes collide with
//! plain TIFF. This decoder claims its extensions with priority instead
//! (see [`ImageFormat::prefer_extension`]). Only CR2 is unambiguous from
//! magic alone.

use std::sync::LazyLock;

use rawler::decoders::{RawDecodeParams, RawLoader};
use rawler::rawsource::RawSource;

use super::finish;
use crate::media::registry::{DecodeOpts, ImageFormat};
use crate::media::{DecodedMedia, MediaError};

static LOADER: LazyLock<RawLoader> = LazyLock::new(RawLoader::new);

pub struct Raw;

impl ImageFormat for Raw {
    fn extensions(&self) -> &'static [&'static str] {
        &[
            "cr2", "cr3", "nef", "arw", "dng", "orf", "rw2", "raf", "pef", "srw",
        ]
    }

    fn prefer_extension(&self) -> bool {
        true
    }

    fn sniff(&self, magic: &[u8]) -> bool {
        // CR2: TIFF-LE header followed by the "CR" marker at offset 8.
        magic.len() >= 10 && magic.starts_with(&[0x49, 0x49, 0x2A, 0x00]) && &magic[8..10] == b"CR"
    }

    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<DecodedMedia, MediaError> {
        let source = RawSource::new_from_slice(bytes);
        let decoder = LOADER
            .get_decoder(&source)
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        let params = RawDecodeParams::default();

        // Largest embedded preview wins, fall back to the thumbnail.
        let preview = decoder
            .preview_image(&source, &params)
            .ok()
            .flatten()
            .or_else(|| decoder.thumbnail_image(&source, &params).ok().flatten())
            .ok_or_else(|| MediaError::Decode("no embedded preview in RAW file".into()))?;

        Ok(DecodedMedia::Static(finish(preview, opts)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_cr2_only() {
        let mut cr2 = vec![0x49, 0x49, 0x2A, 0x00, 0x10, 0, 0, 0];
        cr2.extend_from_slice(b"CR\x02\x00");
        assert!(Raw.sniff(&cr2));
        // Plain TIFF must NOT be claimed, that one belongs to the image crate.
        let tiff = [0x49, 0x49, 0x2A, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0];
        assert!(!Raw.sniff(&tiff));
    }

    #[test]
    fn garbage_is_a_decode_error() {
        assert!(Raw.decode(&[0u8; 64], &DecodeOpts::default()).is_err());
    }
}
