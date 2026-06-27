//! Application configuration: persisted settings, pre-fetch depth, and
//! supported image formats.
//!
//! Settings live in `config_dir()/scryglass/config.toml`, or in
//! `<exe>/data/config.toml` for a portable install (see [`data_dir`]). Every
//! field has a serde default so the format can evolve additively: unknown keys
//! are ignored and missing keys fall back to defaults.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

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

/// What resolution a focused window's prefetch neighbors are uploaded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefetchVram {
    /// Full-resolution texture (instant crisp zoom when navigated to).
    FullRes,
    /// Downscaled to the window (smaller VRAM); promoted on navigation.
    #[default]
    ViewRes,
    /// No texture; decoded into RAM only, uploaded on navigation.
    None,
}

/// When a backgrounded window's RAM source is evicted (re-decoded on return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictPolicy {
    /// Never evict; keep the full-res source in RAM.
    Never,
    /// Evict after a fixed delay.
    Fixed(Duration),
    /// Evict after a delay that scales with the image's decode time.
    Dynamic,
}

impl Serialize for EvictPolicy {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            EvictPolicy::Never => s.serialize_str("never"),
            EvictPolicy::Dynamic => s.serialize_str("dynamic"),
            EvictPolicy::Fixed(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for EvictPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "never" => EvictPolicy::Never,
            "dynamic" => EvictPolicy::Dynamic,
            _ => {
                EvictPolicy::Fixed(humantime::parse_duration(&s).map_err(serde::de::Error::custom)?)
            }
        })
    }
}

/// Serde for a `Duration` as a humantime string (`"15s"`, `"200ms"`).
mod humantime_dur {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&humantime::format_duration(*d).to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

/// Serde for an `Option<Duration>` where `None` is the string `"never"`.
mod humantime_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            None => s.serialize_str("never"),
            Some(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let s = String::deserialize(d)?;
        if s.eq_ignore_ascii_case("never") {
            Ok(None)
        } else {
            humantime::parse_duration(&s)
                .map(Some)
                .map_err(serde::de::Error::custom)
        }
    }
}

/// The decay pipeline a backgrounded window runs: full-res VRAM, then
/// (after each timer) demote to view-res, drop the VRAM, and evict the RAM
/// source. A `None` timer skips that stage. Shared by the unfocused and
/// minimized states so their logic and labels are identical.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecayPipeline {
    /// Demote the on-screen image full-res -> view-res after this delay.
    #[serde(with = "humantime_opt")]
    pub demote_vram_after: Option<Duration>,
    /// Drop the on-screen image's VRAM entirely after this delay.
    #[serde(with = "humantime_opt")]
    pub drop_vram_after: Option<Duration>,
    /// When to evict the RAM source.
    pub evict_ram: EvictPolicy,
    /// Dynamic eviction: delay for an instant-decode image.
    #[serde(with = "humantime_dur")]
    pub evict_ram_min: Duration,
    /// Dynamic eviction: delay for an image at the decode-latency ceiling.
    #[serde(with = "humantime_dur")]
    pub evict_ram_max: Duration,
    /// Dynamic eviction: an image slower than this to decode is never evicted.
    #[serde(with = "humantime_dur")]
    pub max_decode_latency: Duration,
}

impl Default for DecayPipeline {
    /// Conservative fallback for a deleted key: do nothing aggressive. The real
    /// per-state defaults are set in [`ResourceConfig::default`].
    fn default() -> Self {
        Self {
            demote_vram_after: None,
            drop_vram_after: None,
            evict_ram: EvictPolicy::Never,
            evict_ram_min: Duration::from_secs(30),
            evict_ram_max: Duration::from_secs(600),
            max_decode_latency: Duration::from_millis(200),
        }
    }
}

impl DecayPipeline {
    /// The RAM-eviction delay for an image given its decode time, or `None` to
    /// never evict. Dynamic mode interpolates linearly between `evict_ram_min`
    /// (instant decode) and `evict_ram_max` (at the latency ceiling); an image
    /// at or past the ceiling, or one whose decode time is unknown, is kept.
    pub fn evict_delay(&self, decode: Option<Duration>) -> Option<Duration> {
        match self.evict_ram {
            EvictPolicy::Never => None,
            EvictPolicy::Fixed(d) => Some(d),
            EvictPolicy::Dynamic => {
                let decode = decode?;
                if decode >= self.max_decode_latency {
                    return None;
                }
                let t = decode.as_secs_f64() / self.max_decode_latency.as_secs_f64();
                let min = self.evict_ram_min.as_secs_f64();
                let max = self.evict_ram_max.as_secs_f64();
                Some(Duration::from_secs_f64(min + (max - min) * t))
            }
        }
    }
}

/// The minimized state's pipeline plus its video-pause toggle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MinimizedConfig {
    /// Pause an open video while the window is minimized.
    pub pause_video: bool,
    #[serde(flatten)]
    pub pipeline: DecayPipeline,
}

impl Default for MinimizedConfig {
    fn default() -> Self {
        Self {
            pause_video: true,
            pipeline: DecayPipeline::default(),
        }
    }
}

/// Advanced memory/VRAM resource model. The defaults are scryglass's opinion;
/// every field is tunable in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// What resolution a focused window's prefetch neighbors upload at.
    pub prefetch_vram: PrefetchVram,
    /// Decay pipeline for an unfocused window.
    pub unfocused: DecayPipeline,
    /// Decay pipeline for a minimized window.
    pub minimized: MinimizedConfig,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            prefetch_vram: PrefetchVram::ViewRes,
            unfocused: DecayPipeline {
                demote_vram_after: Some(Duration::from_secs(15)),
                drop_vram_after: None,
                evict_ram: EvictPolicy::Dynamic,
                evict_ram_min: Duration::from_secs(30),
                evict_ram_max: Duration::from_secs(600),
                max_decode_latency: Duration::from_millis(200),
            },
            minimized: MinimizedConfig {
                pause_video: true,
                pipeline: DecayPipeline {
                    demote_vram_after: None,
                    drop_vram_after: Some(Duration::ZERO),
                    evict_ram: EvictPolicy::Dynamic,
                    evict_ram_min: Duration::from_secs(15),
                    evict_ram_max: Duration::from_secs(300),
                    max_decode_latency: Duration::from_millis(200),
                },
            },
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
            show_toolbar: true,
            show_filmstrip: true,
            show_slider: true,
            show_footer: true,
            show_info: false,
            show_checkerboard: false,
            resource: ResourceConfig::default(),
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
/// opt-in marker for a portable build that travels with its folder; otherwise
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
    /// The file exists but is not valid TOML; defaults are used and the original
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
    /// A test helper for exercising the parse behavior; production loads report
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
    /// rename it over the target. A rename is atomic, so a reader (or a second
    /// window saving at the same time) never sees a half-written file, which a
    /// plain write would produce when two windows close at once. Errors are
    /// deliberately swallowed: failing to persist settings must never disturb
    /// the viewer.
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
        // The strict parse surfaces the error; the lenient one falls back.
        assert!(AppConfig::try_from_toml(bad).is_err());
        assert_eq!(AppConfig::from_toml(bad).prefetch_depth, 5);
        // A valid partial document parses, with missing keys defaulted.
        let cfg = AppConfig::try_from_toml("prefetch_depth = 9").unwrap();
        assert_eq!(cfg.prefetch_depth, 9);
        assert_eq!(cfg.cache_budget_mb, 512);
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
            show_toolbar: false,
            show_filmstrip: true,
            show_slider: false,
            show_footer: true,
            show_info: true,
            show_checkerboard: true,
            resource: ResourceConfig {
                prefetch_vram: PrefetchVram::FullRes,
                unfocused: DecayPipeline {
                    demote_vram_after: Some(Duration::from_secs(20)),
                    drop_vram_after: None,
                    evict_ram: EvictPolicy::Fixed(Duration::from_secs(90)),
                    evict_ram_min: Duration::from_secs(25),
                    evict_ram_max: Duration::from_secs(500),
                    max_decode_latency: Duration::from_millis(150),
                },
                minimized: MinimizedConfig {
                    pause_video: false,
                    pipeline: DecayPipeline {
                        demote_vram_after: Some(Duration::from_secs(5)),
                        drop_vram_after: Some(Duration::from_secs(10)),
                        evict_ram: EvictPolicy::Never,
                        evict_ram_min: Duration::from_secs(10),
                        evict_ram_max: Duration::from_secs(120),
                        max_decode_latency: Duration::from_millis(250),
                    },
                },
            },
        };
        assert_eq!(AppConfig::from_toml(&cfg.to_toml()), cfg);
    }

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(AppConfig::default().theme, ThemeChoice::Dark);
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
    fn old_natural_name_sort_value_still_parses() {
        let cfg = AppConfig::from_toml("sort_key = \"NaturalName\"\nshow_footer = false\n");
        assert_eq!(cfg.sort_key, SortKey::Name);
        assert!(!cfg.show_footer);
    }

    #[test]
    fn timers_parse_human_durations_and_never() {
        let cfg = AppConfig::from_toml(
            "[resource.unfocused]\ndemote_vram_after = \"500ms\"\ndrop_vram_after = \"never\"\n",
        );
        assert_eq!(
            cfg.resource.unfocused.demote_vram_after,
            Some(Duration::from_millis(500))
        );
        assert_eq!(cfg.resource.unfocused.drop_vram_after, None);
    }

    #[test]
    fn evict_policy_parses_never_dynamic_and_a_duration() {
        let parse = |s: &str| {
            AppConfig::from_toml(&format!("[resource.unfocused]\nevict_ram = \"{s}\"\n"))
                .resource
                .unfocused
                .evict_ram
        };
        assert_eq!(parse("never"), EvictPolicy::Never);
        assert_eq!(parse("dynamic"), EvictPolicy::Dynamic);
        assert_eq!(parse("2m"), EvictPolicy::Fixed(Duration::from_secs(120)));
    }

    #[test]
    fn evict_delay_never_and_fixed() {
        let mut p = DecayPipeline::default();
        assert_eq!(p.evict_delay(Some(Duration::from_millis(10))), None);
        p.evict_ram = EvictPolicy::Fixed(Duration::from_secs(60));
        assert_eq!(
            p.evict_delay(Some(Duration::from_millis(10))),
            Some(Duration::from_secs(60))
        );
        // Fixed ignores decode time, even unknown.
        assert_eq!(p.evict_delay(None), Some(Duration::from_secs(60)));
    }

    #[test]
    fn evict_delay_dynamic_interpolates_and_keeps_slow_or_unknown() {
        let p = DecayPipeline {
            evict_ram: EvictPolicy::Dynamic,
            evict_ram_min: Duration::from_secs(30),
            evict_ram_max: Duration::from_secs(630),
            max_decode_latency: Duration::from_millis(200),
            ..DecayPipeline::default()
        };
        // Instant decode -> min; halfway -> midpoint; at/over ceiling -> never.
        assert_eq!(
            p.evict_delay(Some(Duration::ZERO)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            p.evict_delay(Some(Duration::from_millis(100))),
            Some(Duration::from_secs(330))
        );
        assert_eq!(p.evict_delay(Some(Duration::from_millis(200))), None);
        assert_eq!(p.evict_delay(Some(Duration::from_millis(250))), None);
        assert_eq!(p.evict_delay(None), None);
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

        let doc = include_str!("../docs/advanced-settings.md");
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
