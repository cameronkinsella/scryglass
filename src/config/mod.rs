//! Application configuration: persisted settings, pre-fetch depth, and
//! supported image formats.
//!
//! Settings live in `config_dir()/scryglass/config.toml`, or in
//! `<exe>/data/config.toml` for a portable install (see [`data_dir`]). Every
//! field has a serde default so the format can evolve additively: unknown keys
//! are ignored and missing keys fall back to defaults.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

mod resource;
mod startup;
mod ui;

#[cfg(target_os = "windows")]
pub use resource::WorkingSetTrim;
pub use resource::{
    DecayPipeline, EvictConfig, PrefetchDecay, PrefetchDropAnchor, PrefetchParallelism,
    PrefetchScaler, PrefetchVram, RamBudget, ResourceConfig, VideoDecay, total_system_ram,
};
// Reached as `crate::config::EvictPolicy` only from another module's tests, so a
// plain (test-less) build sees this re-export as unused though it is not.
#[allow(unused_imports)]
pub use resource::EvictPolicy;
pub use startup::{PresentMode, StartupConfig};
pub use ui::{DownscaleKernel, SortKey, ThemeChoice, ZoomMode};

/// Supported image file extensions (lowercase, no dot), collected from
/// the decoder registry so feature flags add/remove formats everywhere
/// (directory scan, archives, file dialog) at once.
static SUPPORTED_EXTENSIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    crate::media::registry::global()
        .extensions()
        .chain(crate::video::EXTENSIONS.iter().copied())
        .collect()
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Number of images to pre-fetch in each direction.
    pub prefetch_depth: usize,
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
    /// Kernel used to shrink stills and animations to fit.
    pub downscale_kernel: DownscaleKernel,
    /// Persist thumbnails on disk between sessions (warm folders open
    /// instantly). Reconciled against deleted files, expired after 90
    /// unused days, size-capped. Requires the `disk-thumbs` build feature.
    pub disk_thumbs: bool,
    /// Pure-viewer mode: all file modification (delete, rename) is
    /// hidden and blocked.
    pub read_only: bool,
    /// Ask before moving a file to the recycle bin.
    pub confirm_delete: bool,
    /// Show click-to-navigate arrows on the left and right image edges.
    pub mouse_nav: bool,
    /// Last window size, restored at startup.
    pub window_width: f32,
    pub window_height: f32,
    /// Last window position. None lets the OS place it (first run).
    pub window_x: Option<f32>,
    pub window_y: Option<f32>,
    /// Whether the last window was maximized or fullscreen, replayed on open.
    pub window_maximized: bool,
    pub window_fullscreen: bool,
    /// Video playback volume (0-1) and mute, persisted across sessions.
    pub video_volume: f32,
    pub video_muted: bool,
    /// Whether video playback loops, persisted across sessions.
    pub video_loop: bool,
    /// Decode video on the GPU when the platform and codec support it,
    /// falling back to software automatically. Disable to force software.
    pub hardware_decode: bool,
    /// Downscale a minified video with the factor-aware kernel (matching the
    /// still-image quality) instead of one bilinear tap. Disable to cut the
    /// per-frame GPU cost on a shrunk-to-fit video.
    pub video_high_quality_scaling: bool,
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
    /// Advanced memory/VRAM resource model (see `docs/advanced-settings.md`).
    pub resource: ResourceConfig,
    /// Settings applied once at launch. Changing them needs a full restart.
    pub startup: StartupConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            prefetch_depth: 5,
            theme: ThemeChoice::default(),
            zoom_mode: ZoomMode::default(),
            sort_key: SortKey::default(),
            sort_desc: false,
            crisp_pixels: false,
            downscale_kernel: DownscaleKernel::default(),
            disk_thumbs: true,
            read_only: false,
            confirm_delete: true,
            mouse_nav: true,
            window_width: 1024.0,
            window_height: 768.0,
            window_x: None,
            window_y: None,
            window_maximized: false,
            window_fullscreen: false,
            video_volume: 1.0,
            video_muted: false,
            video_loop: false,
            hardware_decode: true,
            video_high_quality_scaling: true,
            show_toolbar: true,
            show_filmstrip: true,
            show_slider: true,
            show_footer: true,
            show_info: false,
            show_checkerboard: false,
            resource: ResourceConfig::default(),
            startup: StartupConfig::default(),
        }
    }
}

/// Where the app keeps its data: beside the executable (portable), or in the
/// per-user OS directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDir {
    /// `<exe>/data` exists and is writable: config and thumbnails live there, so
    /// the install is portable and trace-free.
    Portable(PathBuf),
    /// `<exe>/data` exists but is not writable: fall back to the OS dirs, warn.
    PortableReadOnly,
    /// No portable folder: use the per-user OS directories.
    System,
}

/// Resolve the data location once. A `data/` folder beside the executable is the
/// opt-in marker for a portable build that travels with its folder. Otherwise
/// the per-user OS directories are used.
pub fn data_dir() -> &'static DataDir {
    static DATA_DIR: LazyLock<DataDir> = LazyLock::new(|| {
        let Some(data) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.join("data")))
        else {
            return DataDir::System;
        };
        if !data.is_dir() {
            DataDir::System
        } else if dir_is_writable(&data) {
            DataDir::Portable(data)
        } else {
            DataDir::PortableReadOnly
        }
    });
    &DATA_DIR
}

/// Whether `dir` accepts a new file, tested by creating and removing a probe.
fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(".scryglass-write-test");
    std::fs::File::create(&probe)
        .map(|_| {
            let _ = std::fs::remove_file(&probe);
        })
        .is_ok()
}

/// The outcome of loading the persisted config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLoad {
    /// Parsed cleanly (missing keys took their defaults).
    Ok,
    /// No config file yet (first run).
    Missing,
    /// The file exists but is not valid TOML. Defaults are used and the original
    /// is preserved.
    Malformed,
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

    /// Location of the persisted config file: `<exe>/data/config.toml` in a
    /// portable install, else `config_dir()/scryglass/config.toml`.
    pub fn path() -> Option<PathBuf> {
        match data_dir() {
            DataDir::Portable(data) => Some(data.join("config.toml")),
            DataDir::System | DataDir::PortableReadOnly => {
                dirs::config_dir().map(|d| d.join("scryglass").join("config.toml"))
            }
        }
    }

    /// Load the persisted config, reporting the outcome so a malformed file is
    /// preserved and the user warned, rather than silently reset to defaults
    /// (which the next save would then overwrite, losing their edits).
    pub fn load_reporting() -> (Self, ConfigLoad) {
        let Some(path) = Self::path() else {
            return (Self::default(), ConfigLoad::Missing);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse_reporting(&text),
            Err(_) => (Self::default(), ConfigLoad::Missing),
        }
    }

    /// Classify config text: cleanly parsed (missing keys default) versus
    /// malformed (defaults used). The pure half of [`load_reporting`].
    fn parse_reporting(text: &str) -> (Self, ConfigLoad) {
        match Self::try_from_toml(text) {
            Ok(cfg) => (cfg, ConfigLoad::Ok),
            Err(_) => (Self::default(), ConfigLoad::Malformed),
        }
    }

    /// Copy a malformed config aside to `config.toml.bak`, so a hand-edit typo
    /// never costs the user their settings. Best-effort: a failure to back up
    /// must not block startup.
    pub fn backup_malformed() {
        if let Some(path) = Self::path() {
            let _ = std::fs::copy(&path, path.with_extension("toml.bak"));
        }
    }

    /// Parse a TOML document leniently: unknown keys are ignored, missing keys
    /// take their defaults, and a malformed document yields the full defaults.
    /// A test helper for exercising the parse behavior. Production loads report
    /// the outcome via [`load_reporting`].
    #[cfg(test)]
    pub fn from_toml(s: &str) -> Self {
        Self::try_from_toml(s).unwrap_or_default()
    }

    /// Parse a TOML document, surfacing a syntax error instead of falling back
    /// to defaults. Used for reload and validation, where a silent reset would
    /// discard the user's settings. Missing keys still take their defaults.
    pub fn try_from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a TOML document.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Write the config to disk atomically: write a unique temp file, then
    /// rename it over the target. A reader (or a second window saving at the
    /// same time) never sees the half-written file a plain write would produce
    /// when two windows close at once. Errors are deliberately swallowed:
    /// failing to persist settings must never disturb the viewer.
    pub async fn save(self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("toml.{seq}.tmp"));
        // Sync before the rename: without it a crash right after can swap the
        // name to a file whose data never reached the disk.
        let written = async {
            use tokio::io::AsyncWriteExt as _;
            let mut file = tokio::fs::File::create(&tmp).await?;
            file.write_all(self.to_toml().as_bytes()).await?;
            file.sync_all().await
        }
        .await;
        if written.is_ok() && tokio::fs::rename(&tmp, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

/// Disambiguates concurrent saves' temp files (window closes race in one
/// process), so each writes its own temp before the atomic rename.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[cfg(target_os = "windows")]
    use super::resource::WorkingSetConfig;
    use super::resource::{MinimizedConfig, StateDecay};
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
    fn parse_reporting_flags_malformed_but_keeps_valid() {
        let (cfg, status) = AppConfig::parse_reporting("prefetch_depth = 9");
        assert_eq!(status, ConfigLoad::Ok);
        assert_eq!(cfg.prefetch_depth, 9);

        let (cfg, status) = AppConfig::parse_reporting("prefetch_depth = = bad");
        assert_eq!(status, ConfigLoad::Malformed);
        assert_eq!(cfg.prefetch_depth, 5); // untouched defaults
    }

    #[test]
    fn try_from_toml_surfaces_errors_where_from_toml_defaults() {
        let bad = "prefetch_depth = = nonsense";
        // The strict parse surfaces the error. The lenient one falls back.
        assert!(AppConfig::try_from_toml(bad).is_err());
        assert_eq!(AppConfig::from_toml(bad).prefetch_depth, 5);
        // A valid partial document parses, with missing keys defaulted.
        let cfg = AppConfig::try_from_toml("prefetch_depth = 9").unwrap();
        assert_eq!(cfg.prefetch_depth, 9);
    }

    #[test]
    fn toml_roundtrip_preserves_all_fields() {
        let cfg = AppConfig {
            prefetch_depth: 3,
            theme: ThemeChoice::Light,
            zoom_mode: ZoomMode::ScaleToFit,
            sort_key: SortKey::DateModified,
            sort_desc: true,
            crisp_pixels: true,
            downscale_kernel: DownscaleKernel::Lanczos3,
            disk_thumbs: false,
            read_only: true,
            confirm_delete: false,
            mouse_nav: false,
            window_width: 640.0,
            window_height: 480.0,
            window_x: Some(100.0),
            window_y: Some(50.0),
            window_maximized: true,
            window_fullscreen: true,
            video_volume: 0.5,
            video_muted: true,
            video_loop: true,
            hardware_decode: false,
            video_high_quality_scaling: false,
            show_toolbar: false,
            show_filmstrip: true,
            show_slider: false,
            show_footer: true,
            show_info: true,
            show_checkerboard: true,
            resource: ResourceConfig {
                prefetch_vram: PrefetchVram::FullRes,
                prefetch_scaler: PrefetchScaler::Cpu,
                prefetch_parallelism: PrefetchParallelism::Fixed(3),
                large_image_ram_budget: RamBudget::Bytes(2_000_000_000),
                unfocused: StateDecay {
                    still: DecayPipeline {
                        demote_vram_after: Some(Duration::from_secs(20)),
                        drop_vram_after: None,
                        evict: EvictConfig {
                            evict_ram: EvictPolicy::Fixed(Duration::from_secs(90)),
                            evict_ram_min: Duration::from_secs(25),
                            evict_ram_max: Duration::from_secs(500),
                            max_decode_latency: Duration::from_millis(150),
                        },
                    },
                    animated: EvictConfig {
                        evict_ram: EvictPolicy::Fixed(Duration::from_secs(45)),
                        ..EvictConfig::default()
                    },
                    video: VideoDecay {
                        evict_session_after: None,
                    },
                    prefetch: PrefetchDecay {
                        drop_on: PrefetchDropAnchor::Evict,
                        drop_after: Duration::from_secs(7),
                        drop_interval: Duration::from_secs(3),
                    },
                },
                minimized: MinimizedConfig {
                    pause_video: false,
                    still: DecayPipeline {
                        demote_vram_after: Some(Duration::from_secs(5)),
                        drop_vram_after: Some(Duration::from_secs(10)),
                        evict: EvictConfig {
                            evict_ram: EvictPolicy::Never,
                            evict_ram_min: Duration::from_secs(10),
                            evict_ram_max: Duration::from_secs(120),
                            max_decode_latency: Duration::from_millis(250),
                        },
                    },
                    animated: EvictConfig {
                        evict_ram: EvictPolicy::Dynamic,
                        ..EvictConfig::default()
                    },
                    video: VideoDecay {
                        evict_session_after: Some(Duration::from_secs(8)),
                    },
                    prefetch: PrefetchDecay {
                        drop_on: PrefetchDropAnchor::Drop,
                        drop_after: Duration::from_secs(2),
                        drop_interval: Duration::from_secs(1),
                    },
                },
                #[cfg(target_os = "windows")]
                working_set: WorkingSetConfig {
                    trim_when: WorkingSetTrim::AllMinimized,
                    trim_after: Duration::from_secs(12),
                },
            },
            startup: StartupConfig {
                present_mode: PresentMode::NoVsync,
            },
        };
        assert_eq!(AppConfig::from_toml(&cfg.to_toml()), cfg);
    }

    #[test]
    fn mouse_nav_is_on_by_default() {
        assert!(AppConfig::default().mouse_nav);
    }

    #[test]
    fn from_toml_ignores_unknown_keys() {
        let cfg = AppConfig::from_toml("some_future_setting = 42\nprefetch_depth = 7\n");
        assert_eq!(cfg.prefetch_depth, 7);
    }

    #[test]
    fn every_config_key_is_documented() {
        fn collect_keys(value: &toml::Value, out: &mut std::collections::BTreeSet<String>) {
            if let toml::Value::Table(table) = value {
                for (key, child) in table {
                    out.insert(key.clone());
                    collect_keys(child, out);
                }
            }
        }
        let toml_str = AppConfig::default().to_toml();
        let value: toml::Value = toml::from_str(&toml_str).unwrap();
        let mut keys = std::collections::BTreeSet::new();
        collect_keys(&value, &mut keys);

        let doc = include_str!("../../docs/advanced-settings.md");
        for key in &keys {
            assert!(
                doc.contains(key.as_str()),
                "config key `{key}` is not documented in docs/advanced-settings.md"
            );
        }
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
