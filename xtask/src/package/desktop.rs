//! The Linux `.desktop` entry. Pure text generation so the MIME coverage is
//! unit-tested rather than typed by hand in CI.

/// MIME types scryglass claims, across images, video, and comic archives.
const MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/bmp",
    "image/webp",
    "image/tiff",
    "image/vnd.microsoft.icon",
    "image/gif",
    "image/avif",
    "image/heic",
    "image/heif",
    "image/jxl",
    "image/svg+xml",
    "image/x-canon-cr2",
    "image/x-canon-cr3",
    "image/x-nikon-nef",
    "image/x-sony-arw",
    "image/x-adobe-dng",
    "image/x-olympus-orf",
    "image/x-panasonic-rw2",
    "image/x-fuji-raf",
    "image/x-pentax-pef",
    "image/x-samsung-srw",
    "video/mp4",
    "video/x-matroska",
    "video/webm",
    "video/quicktime",
    "video/x-msvideo",
    "video/x-m4v",
    "application/vnd.comicbook+zip",
    "application/vnd.comicbook-rar",
    "application/x-cb7",
];

/// The full `.desktop` file contents.
#[allow(dead_code)] // used only by the Linux builder, but tested everywhere
pub fn entry() -> String {
    let mime = MIME_TYPES.join(";");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=scryglass\n\
         Exec=scryglass %f\n\
         Icon=scryglass\n\
         Categories=Graphics;Viewer;\n\
         MimeType={mime};\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_well_formed_and_covers_each_family() {
        let entry = entry();
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=scryglass %f"));
        // A representative type from each family must be present.
        assert!(entry.contains("image/png"));
        assert!(entry.contains("image/jxl"));
        assert!(entry.contains("image/x-canon-cr2"));
        assert!(entry.contains("video/mp4"));
        assert!(entry.contains("application/vnd.comicbook+zip"));
        // MimeType line ends with a trailing semicolon, per the spec.
        let mime_line = entry.lines().find(|l| l.starts_with("MimeType=")).unwrap();
        assert!(mime_line.ends_with(';'));
    }
}
