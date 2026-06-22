//! Application configuration: persisted settings, pre-fetch depth, and
//! supported image formats.
//!
//! Settings live in `config_dir()/scryglass/config.toml`. Every field has a
//! serde default so the format can evolve additively: unknown keys are
//! ignored and missing keys fall back to defaults.

use std::path::PathBuf;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// Supported image file extensions (lowercase, no dot), collected from
/// the decoder registry so feature flags add/remove formats everywhere
/// (directory scan, archives, file dialog) at once.
static SUPPORTED_EXTENSIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    crate::media::registry::global()
        .extensions()
        .chain(crate::video::EXTENSIONS.iter().copied())
        .collect()
});

/// Which color theme the UI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeChoice {
    /// Near-black chrome designed for photo viewing.
    #[default]
    Dark,
    /// Bright chrome for well-lit environments.
    Light,
}

/// How the file list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SortKey {
    /// Name order matching the platform's file manager. The alias keeps
    /// configs written before the two name sorts were merged.
    #[default]
    #[serde(alias = "NaturalName")]
    Name,
    /// Most recently modified last (or first when descending).
    DateModified,
    /// Smallest first (or largest when descending).
    Size,
}

impl SortKey {
    /// All keys in menu order.
    pub const ALL: &'static [SortKey] = &[SortKey::Name, SortKey::DateModified, SortKey::Size];

    /// Human-readable label for menu display.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::DateModified => "Date modified",
            SortKey::Size => "Size",
        }
    }
}

/// How the image zoom level is determined when opening/navigating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Number of images to pre-fetch in each direction.
    pub prefetch_depth: usize,
    /// GPU image cache budget in megabytes.
    pub cache_budget_mb: usize,
    /// Active color theme.
    pub theme: ThemeChoice,
    /// Zoom mode applied when opening/navigating images.
    pub zoom_mode: ZoomMode,
    /// How the file list is ordered.
    pub sort_key: SortKey,
    /// Reverse the sort order.
    pub sort_desc: bool,
    /// Nearest-neighbor sampling past 100% zoom: crisp pixels for pixel art.
    pub crisp_pixels: bool,
    /// Persist thumbnails on disk between sessions (warm folders open
    /// instantly). Reconciled against deleted files, expired after 90
    /// unused days, size-capped. Requires the `disk-thumbs` build feature.
    pub disk_thumbs: bool,
    /// Pure-viewer mode: all file modification (delete, rename) is
    /// hidden and blocked.
    pub read_only: bool,
    /// Ask before moving a file to the recycle bin.
    pub confirm_delete: bool,
    /// Last window size, restored at startup.
    pub window_width: f32,
    pub window_height: f32,
    /// Video playback volume (0-1) and mute, persisted across sessions.
    pub video_volume: f32,
    pub video_muted: bool,
    /// Whether video playback loops, persisted across sessions.
    pub video_loop: bool,
    /// Decode video on the GPU when the platform and codec support it,
    /// falling back to software automatically. Disable to force software.
    pub hardware_decode: bool,
    /// Whether the toolbar is visible.
    pub show_toolbar: bool,
    /// Whether the filmstrip is visible.
    pub show_filmstrip: bool,
    /// Whether the navigation slider is visible.
    pub show_slider: bool,
    /// Whether the footer is visible.
    pub show_footer: bool,
    /// Whether the info panel (file details + EXIF) is visible.
    pub show_info: bool,
    /// Draw a checkerboard behind images (reveals transparency).
    pub show_checkerboard: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            prefetch_depth: 5,
            cache_budget_mb: 512,
            theme: ThemeChoice::default(),
            zoom_mode: ZoomMode::default(),
            sort_key: SortKey::default(),
            sort_desc: false,
            crisp_pixels: false,
            disk_thumbs: true,
            read_only: false,
            confirm_delete: true,
            window_width: 1024.0,
            window_height: 768.0,
            video_volume: 1.0,
            video_muted: false,
            video_loop: false,
            hardware_decode: true,
            show_toolbar: true,
            show_filmstrip: true,
            show_slider: true,
            show_footer: true,
            show_info: false,
            show_checkerboard: false,
        }
    }
}

impl AppConfig {
    /// Returns true if `ext` (without leading dot) is a supported image format.
    pub fn is_supported_extension(ext: &str) -> bool {
        SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    }

    /// Returns the list of supported extensions (for file dialog filters).
    pub fn supported_extensions() -> &'static [&'static str] {
        &SUPPORTED_EXTENSIONS
    }

    /// Location of the persisted config file, if a config dir exists.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("scryglass").join("config.toml"))
    }

    /// Load the persisted config, falling back to defaults if the file is
    /// missing or unreadable.
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| Self::from_toml(&s))
            .unwrap_or_default()
    }

    /// Parse a TOML document. Unknown keys are ignored, missing keys take
    /// their defaults, and a malformed document yields the full defaults.
    pub fn from_toml(s: &str) -> Self {
        toml::from_str(s).unwrap_or_default()
    }

    /// Serialize to a TOML document.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Write the config to disk. Errors are deliberately swallowed,
    /// failing to persist settings must never disturb the viewer.
    pub async fn save(self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&path, self.to_toml()).await;
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
    fn default_shows_all_chrome() {
        let cfg = AppConfig::default();
        assert!(cfg.show_toolbar);
        assert!(cfg.show_filmstrip);
        assert!(cfg.show_slider);
        assert!(cfg.show_footer);
    }

    #[test]
    fn toml_roundtrip_preserves_all_fields() {
        let cfg = AppConfig {
            prefetch_depth: 3,
            cache_budget_mb: 256,
            theme: ThemeChoice::Light,
            zoom_mode: ZoomMode::ScaleToFit,
            sort_key: SortKey::DateModified,
            sort_desc: true,
            crisp_pixels: true,
            disk_thumbs: false,
            read_only: true,
            confirm_delete: false,
            window_width: 640.0,
            window_height: 480.0,
            video_volume: 0.5,
            video_muted: true,
            video_loop: true,
            hardware_decode: false,
            show_toolbar: false,
            show_filmstrip: true,
            show_slider: false,
            show_footer: true,
            show_info: true,
            show_checkerboard: true,
        };
        assert_eq!(AppConfig::from_toml(&cfg.to_toml()), cfg);
    }

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(AppConfig::default().theme, ThemeChoice::Dark);
    }

    #[test]
    fn from_toml_ignores_unknown_keys() {
        let cfg = AppConfig::from_toml("some_future_setting = 42\nprefetch_depth = 7\n");
        assert_eq!(cfg.prefetch_depth, 7);
    }

    #[test]
    fn old_natural_name_sort_value_still_parses() {
        let cfg = AppConfig::from_toml("sort_key = \"NaturalName\"\nshow_footer = false\n");
        assert_eq!(cfg.sort_key, SortKey::Name);
        assert!(!cfg.show_footer);
    }

    #[test]
    fn from_toml_defaults_missing_keys() {
        let cfg = AppConfig::from_toml("show_footer = false\n");
        assert!(!cfg.show_footer);
        assert_eq!(cfg.prefetch_depth, 5);
        assert_eq!(cfg.zoom_mode, ZoomMode::Auto);
        assert!(cfg.show_toolbar);
    }

    #[test]
    fn from_toml_empty_document_is_default() {
        assert_eq!(AppConfig::from_toml(""), AppConfig::default());
    }

    #[test]
    fn from_toml_malformed_document_is_default() {
        assert_eq!(
            AppConfig::from_toml("not valid toml ["),
            AppConfig::default()
        );
    }

    #[test]
    fn zoom_mode_serializes_as_readable_name() {
        let cfg = AppConfig {
            zoom_mode: ZoomMode::LockZoomRatio,
            ..Default::default()
        };
        assert!(cfg.to_toml().contains("LockZoomRatio"));
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
        // Videos only join the list with the `video` feature.
        assert_eq!(
            AppConfig::is_supported_extension("mp4"),
            cfg!(feature = "video")
        );
    }

    #[test]
    fn feature_gated_formats_register_their_extensions() {
        #[cfg(feature = "jxl")]
        assert!(AppConfig::is_supported_extension("jxl"));
        #[cfg(feature = "svg")]
        assert!(AppConfig::is_supported_extension("svg"));
        #[cfg(feature = "raw")]
        {
            assert!(AppConfig::is_supported_extension("cr2"));
            assert!(AppConfig::is_supported_extension("nef"));
            assert!(AppConfig::is_supported_extension("dng"));
        }
    }
}
