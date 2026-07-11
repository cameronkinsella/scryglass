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
    PrefetchScaler, PrefetchVram, RamBudget, ResourceConfig, StateDecayRef, total_system_ram,
};
// Reached as `crate::config::EvictPolicy` only from another module's tests, so a
// plain (test-less) build sees this re-export as unused though it is not.
#[allow(unused_imports)]
pub use resource::EvictPolicy;
// The present probe reaches this as `crate::config::PresentMode` only on
// Windows, and the roundtrip test on every platform, so a plain Linux build
// sees the re-export as unused though it is not.
#[allow(unused_imports)]
pub use startup::PresentMode;
pub use startup::StartupConfig;
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

/// The persisted settings: three top-level tiers, each a TOML table tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    /// Settings with an in-app control. Editing the file reaches the same values.
    pub standard: StandardConfig,
    /// Settings only changeable by editing the file.
    pub advanced: AdvancedConfig,
    /// State the app writes for itself, rarely edited by hand.
    pub managed: ManagedConfig,
}

/// The in-app configurable settings, one topical table each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StandardConfig {
    pub browsing: BrowsingConfig,
    pub display: DisplayConfig,
    pub files: FilesConfig,
    pub video: VideoConfig,
    pub chrome: ChromeConfig,
}

/// The file-only settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Scaling quality for images and video.
    pub scaling: ScalingConfig,
    /// Memory/VRAM resource model (see `docs/advanced-settings.md`).
    pub resource: ResourceConfig,
    /// Settings applied once at launch. Changing them needs a full restart.
    pub startup: StartupConfig,
}

/// State the app manages automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ManagedConfig {
    /// Last window geometry, replayed on open.
    pub window: WindowConfig,
}

/// Browsing and cache. In-app configurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowsingConfig {
    /// Number of images to pre-fetch in each direction.
    pub prefetch_depth: usize,
    /// Persist thumbnails on disk between sessions (warm folders open
    /// instantly). Reconciled against deleted files, expired after 90
    /// unused days, size-capped. Requires the `disk-thumbs` build feature.
    pub disk_thumbs: bool,
}

impl Default for BrowsingConfig {
    fn default() -> Self {
        Self {
            prefetch_depth: 5,
            disk_thumbs: true,
        }
    }
}

/// Display and sorting. In-app configurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DisplayConfig {
    /// Active color theme.
    pub theme: ThemeChoice,
    /// Zoom mode applied when opening/navigating images.
    pub zoom_mode: ZoomMode,
    /// How the file list is ordered.
    pub sort_key: SortKey,
    /// Reverse the sort order.
    pub sort_desc: bool,
    /// Nearest-neighbor sampling past 100% zoom: crisp pixels for pixel art.
    pub nearest_neighbor_zoom: bool,
    /// Draw a checkerboard behind images to reveal transparency.
    pub checkerboard: bool,
}

/// File operations. In-app configurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilesConfig {
    /// Pure-viewer mode: all file modification (delete, rename) is
    /// hidden and blocked.
    pub read_only: bool,
    /// Ask before moving a file to the recycle bin.
    pub confirm_delete: bool,
    /// Use the mouse back/forward buttons to navigate.
    pub mouse_nav: bool,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            read_only: false,
            confirm_delete: true,
            mouse_nav: true,
        }
    }
}

/// Video playback. In-app configurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    /// Playback volume (0-1), persisted across sessions.
    pub volume: f32,
    /// Start muted.
    pub muted: bool,
    /// Loop playback. Named `looping` because `loop` is a keyword.
    #[serde(rename = "loop")]
    pub looping: bool,
    /// Decode video on the GPU when the platform and codec support it,
    /// falling back to software automatically. Disable to force software.
    pub hardware_decode: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
            looping: false,
            hardware_decode: true,
        }
    }
}

/// Chrome visibility. In-app configurable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromeConfig {
    /// Show the top toolbar.
    pub toolbar: bool,
    /// Show the thumbnail filmstrip.
    pub filmstrip: bool,
    /// Show the navigation slider.
    pub slider: bool,
    /// Show the footer status bar.
    pub footer: bool,
    /// Show the info panel (file details + EXIF).
    pub info: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            toolbar: true,
            filmstrip: true,
            slider: true,
            footer: true,
            info: false,
        }
    }
}

/// Scaling quality. File only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScalingConfig {
    /// Kernel used to shrink stills and animations to fit.
    pub downscale_kernel: DownscaleKernel,
    /// Downscale a minified video with the factor-aware kernel, matching the
    /// still-image quality. Disable to cut the per-frame GPU cost on a
    /// shrunk-to-fit video.
    pub video_high_quality_scaling: bool,
}

impl Default for ScalingConfig {
    fn default() -> Self {
        Self {
            downscale_kernel: DownscaleKernel::default(),
            video_high_quality_scaling: true,
        }
    }
}

/// Last window geometry. Managed automatically, rarely edited by hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// Last window size, restored at startup.
    pub width: f32,
    pub height: f32,
    /// Last window position. None lets the OS place it (first run).
    pub x: Option<f32>,
    pub y: Option<f32>,
    /// Whether the last window was maximized or fullscreen, replayed on open.
    pub maximized: bool,
    pub fullscreen: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            x: None,
            y: None,
            maximized: false,
            fullscreen: false,
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

    /// Write the config to disk atomically and in order: write a unique temp
    /// file, then rename it over the target. A reader (or a second window
    /// saving at the same time) never sees the half-written file a plain
    /// write would produce when two windows close at once. Concurrent saves
    /// write one at a time, and a save that lost the race to a newer one
    /// skips its write, since renaming a stale snapshot over a fresher file
    /// would hand the live config watcher old settings to re-apply. Errors
    /// are deliberately swallowed: failing to persist settings must never
    /// disturb the viewer.
    pub fn save(self) -> impl Future<Output = ()> + Send + 'static {
        // Take the ticket here, not in the future: callers fire saves on the
        // update thread right after changing the config, so ticket order is
        // snapshot order even when the writes are polled out of order.
        let seq = SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        async move {
            let Some(path) = Self::path() else {
                return;
            };
            self.save_at(&path, seq).await;
        }
    }

    /// The write half of [`AppConfig::save`], aimed at an explicit path so
    /// tests can exercise the ordering without touching the real config.
    async fn save_at(self, path: &Path, seq: u64) {
        // One write at a time, so renames cannot land out of ticket order.
        let _turn = SAVE_LOCK.lock().await;
        // A newer snapshot already reached this path. Writing the stale one
        // would revert it.
        if latest_written(path) > seq {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
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
        if written.is_ok() && tokio::fs::rename(&tmp, path).await.is_ok() {
            mark_written(path, seq);
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

/// Save ticket counter. Orders concurrent saves by fire time and gives each
/// its own temp file name before the atomic rename.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Serializes save writes within the process.
static SAVE_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// The newest ticket whose write reached each path, so a slower older save
/// knows to skip. Keyed by path only so tests on temp files stay independent.
static LAST_WRITTEN: LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>> =
    LazyLock::new(Default::default);

fn latest_written(path: &Path) -> u64 {
    LAST_WRITTEN
        .lock()
        .map(|written| written.get(path).copied().unwrap_or(0))
        .unwrap_or(0)
}

fn mark_written(path: &Path, seq: u64) {
    if let Ok(mut written) = LAST_WRITTEN.lock() {
        written.insert(path.to_path_buf(), seq);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[cfg(target_os = "windows")]
    use super::resource::WorkingSetConfig;
    use super::resource::{MinimizedConfig, MinimizedVideoDecay, StateDecay, VideoDecay};
    use super::*;

    #[test]
    fn default_prefetch_depth_is_5() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.standard.browsing.prefetch_depth, 5);
    }

    #[test]
    fn default_shows_all_chrome() {
        let cfg = AppConfig::default();
        assert!(cfg.standard.chrome.toolbar);
        assert!(cfg.standard.chrome.filmstrip);
        assert!(cfg.standard.chrome.slider);
        assert!(cfg.standard.chrome.footer);
    }

    #[test]
    fn parse_reporting_flags_malformed_but_keeps_valid() {
        let (cfg, status) = AppConfig::parse_reporting("[standard.browsing]\nprefetch_depth = 9");
        assert_eq!(status, ConfigLoad::Ok);
        assert_eq!(cfg.standard.browsing.prefetch_depth, 9);

        let (cfg, status) =
            AppConfig::parse_reporting("[standard.browsing]\nprefetch_depth = = bad");
        assert_eq!(status, ConfigLoad::Malformed);
        assert_eq!(cfg.standard.browsing.prefetch_depth, 5); // untouched defaults
    }

    #[test]
    fn try_from_toml_surfaces_errors_where_from_toml_defaults() {
        let bad = "[standard.browsing]\nprefetch_depth = = nonsense";
        // The strict parse surfaces the error. The lenient one falls back.
        assert!(AppConfig::try_from_toml(bad).is_err());
        assert_eq!(
            AppConfig::from_toml(bad).standard.browsing.prefetch_depth,
            5
        );
        // A valid partial document parses, with missing keys defaulted.
        let cfg = AppConfig::try_from_toml("[standard.browsing]\nprefetch_depth = 9").unwrap();
        assert_eq!(cfg.standard.browsing.prefetch_depth, 9);
    }

    #[test]
    fn toml_roundtrip_preserves_all_fields() {
        let cfg = AppConfig {
            standard: StandardConfig {
                browsing: BrowsingConfig {
                    prefetch_depth: 3,
                    disk_thumbs: false,
                },
                display: DisplayConfig {
                    theme: ThemeChoice::Light,
                    zoom_mode: ZoomMode::ScaleToFit,
                    sort_key: SortKey::DateModified,
                    sort_desc: true,
                    nearest_neighbor_zoom: true,
                    checkerboard: true,
                },
                files: FilesConfig {
                    read_only: true,
                    confirm_delete: false,
                    mouse_nav: false,
                },
                video: VideoConfig {
                    volume: 0.5,
                    muted: true,
                    looping: true,
                    hardware_decode: false,
                },
                chrome: ChromeConfig {
                    toolbar: false,
                    filmstrip: true,
                    slider: false,
                    footer: true,
                    info: true,
                },
            },
            advanced: AdvancedConfig {
                scaling: ScalingConfig {
                    downscale_kernel: DownscaleKernel::Lanczos3,
                    video_high_quality_scaling: false,
                },
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
                        video: MinimizedVideoDecay {
                            evict_session_after: Some(Duration::from_secs(8)),
                            pause: false,
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
            },
            managed: ManagedConfig {
                window: WindowConfig {
                    width: 640.0,
                    height: 480.0,
                    x: Some(100.0),
                    y: Some(50.0),
                    maximized: true,
                    fullscreen: true,
                },
            },
        };
        assert_eq!(AppConfig::from_toml(&cfg.to_toml()), cfg);
    }

    #[test]
    fn mouse_nav_is_on_by_default() {
        assert!(AppConfig::default().standard.files.mouse_nav);
    }

    #[test]
    fn from_toml_ignores_unknown_keys() {
        let cfg = AppConfig::from_toml(
            "some_future_setting = 42\n[standard.browsing]\nprefetch_depth = 7\n",
        );
        assert_eq!(cfg.standard.browsing.prefetch_depth, 7);
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
        let cfg = AppConfig::from_toml("[standard.chrome]\nfooter = false\n");
        assert!(!cfg.standard.chrome.footer);
        assert_eq!(cfg.standard.browsing.prefetch_depth, 5);
        assert_eq!(cfg.standard.display.zoom_mode, ZoomMode::Auto);
        assert!(cfg.standard.chrome.toolbar);
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

    /// Take a save ticket the way [`AppConfig::save`] does at fire time.
    fn ticket() -> u64 {
        SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    async fn saved_depth(path: &Path) -> usize {
        let text = tokio::fs::read_to_string(path).await.unwrap();
        AppConfig::try_from_toml(&text)
            .unwrap()
            .standard
            .browsing
            .prefetch_depth
    }

    #[tokio::test]
    async fn a_stale_save_cannot_overwrite_a_newer_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let older = AppConfig::default();
        let mut newer = AppConfig::default();
        newer.standard.browsing.prefetch_depth = 9;

        // Tickets follow fire order, but the older write lands last.
        let older_seq = ticket();
        let newer_seq = ticket();
        newer.save_at(&path, newer_seq).await;
        older.save_at(&path, older_seq).await;

        assert_eq!(saved_depth(&path).await, 9);
    }

    #[tokio::test]
    async fn in_order_saves_apply_the_newest_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");

        let older = AppConfig::default();
        let mut newer = AppConfig::default();
        newer.standard.browsing.prefetch_depth = 7;

        let older_seq = ticket();
        let newer_seq = ticket();
        older.save_at(&path, older_seq).await;
        newer.save_at(&path, newer_seq).await;

        assert_eq!(saved_depth(&path).await, 7);
    }
}
