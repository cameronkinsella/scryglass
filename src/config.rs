//! Application configuration: pre-fetch depth and supported image formats.

/// Supported image file extensions (lowercase, no dot).
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif", "ico", "avif",
];

/// How the image zoom level is determined when opening/navigating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZoomMode {
    /// 100% if it fits, shrink to fit if too large. Never scale up.
    #[default]
    Auto,
    /// Same initial rules as Auto, but zoom is preserved across navigation.
    LockZoomRatio,
    /// Scale until the image width fills the window width.
    ScaleToWidth,
    /// Scale until the image height fills the window height.
    ScaleToHeight,
    /// Scale until the image fits entirely (no overflow on either axis).
    ScaleToFit,
    /// Scale until both axes fill the window (may overflow one axis).
    ScaleToFill,
}

impl ZoomMode {
    /// All modes in menu order.
    pub const ALL: &'static [ZoomMode] = &[
        ZoomMode::Auto,
        ZoomMode::LockZoomRatio,
        ZoomMode::ScaleToWidth,
        ZoomMode::ScaleToHeight,
        ZoomMode::ScaleToFit,
        ZoomMode::ScaleToFill,
    ];

    /// Human-readable label for menu display.
    pub fn label(self) -> &'static str {
        match self {
            ZoomMode::Auto => "Auto",
            ZoomMode::LockZoomRatio => "Lock Zoom Ratio",
            ZoomMode::ScaleToWidth => "Scale to Width",
            ZoomMode::ScaleToHeight => "Scale to Height",
            ZoomMode::ScaleToFit => "Scale to Fit",
            ZoomMode::ScaleToFill => "Scale to Fill",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Number of images to pre-fetch in each direction.
    pub prefetch_depth: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { prefetch_depth: 5 }
    }
}

impl AppConfig {
    /// Returns true if `ext` (without leading dot) is a supported image format.
    pub fn is_supported_extension(ext: &str) -> bool {
        SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    }

    /// Returns the list of supported extensions (for file dialog filters).
    pub fn supported_extensions() -> &'static [&'static str] {
        SUPPORTED_EXTENSIONS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prefetch_depth_is_5() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.prefetch_depth, 5);
    }

    #[test]
    fn supported_extension_matches_common_formats() {
        assert!(AppConfig::is_supported_extension("png"));
        assert!(AppConfig::is_supported_extension("jpg"));
        assert!(AppConfig::is_supported_extension("jpeg"));
        assert!(AppConfig::is_supported_extension("gif"));
        assert!(AppConfig::is_supported_extension("bmp"));
        assert!(AppConfig::is_supported_extension("webp"));
        assert!(AppConfig::is_supported_extension("tiff"));
        assert!(AppConfig::is_supported_extension("tif"));
        assert!(AppConfig::is_supported_extension("avif"));
    }

    #[test]
    fn supported_extension_is_case_insensitive() {
        assert!(AppConfig::is_supported_extension("PNG"));
        assert!(AppConfig::is_supported_extension("Jpg"));
        assert!(AppConfig::is_supported_extension("WEBP"));
    }

    #[test]
    fn unsupported_extensions_are_rejected() {
        assert!(!AppConfig::is_supported_extension("txt"));
        assert!(!AppConfig::is_supported_extension("rs"));
        assert!(!AppConfig::is_supported_extension("exe"));
        assert!(!AppConfig::is_supported_extension("mp4"));
    }
}
