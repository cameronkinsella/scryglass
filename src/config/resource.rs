//! Advanced memory/VRAM resource model: prefetch tiers, decay timers, the
//! per-state eviction policies, and the RAM budget.

use std::sync::LazyLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What resolution a focused window's prefetch neighbors are uploaded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefetchVram {
    /// Full-resolution texture (instant crisp zoom when navigated to).
    FullRes,
    /// Downscaled to the window (smaller VRAM). Promoted on navigation.
    #[default]
    ViewRes,
    /// No texture. Decoded into RAM only, uploaded on navigation.
    None,
}

/// How many prefetch neighbors decode at once: `"auto"` (half the logical
/// cores, at least 2) or a fixed count. Bounds the CPU burst and the peak
/// transient RAM of in-flight decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrefetchParallelism {
    #[default]
    Auto,
    Fixed(u32),
}

impl PrefetchParallelism {
    /// The concrete permit count for this machine.
    pub fn resolve(&self) -> usize {
        match self {
            Self::Auto => std::thread::available_parallelism()
                .map(|n| (n.get() / 2).max(2))
                .unwrap_or(2),
            Self::Fixed(n) => (*n).max(1) as usize,
        }
    }
}

impl std::fmt::Display for PrefetchParallelism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Fixed(n) => write!(f, "{n}"),
        }
    }
}

impl std::str::FromStr for PrefetchParallelism {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse::<u32>()
            .map(Self::Fixed)
            .map_err(|_| format!("expected \"auto\" or a count, got {s:?}"))
    }
}

impl Serialize for PrefetchParallelism {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PrefetchParallelism {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Where a prefetch neighbor's view-resolution copy is produced. A GPU bake
/// and the CPU resample give identical pixels. They trade transient VRAM
/// against seconds of background CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefetchScaler {
    /// Upload the full decode, render the copy through the display shader,
    /// drop the full texture (briefly holds it in VRAM).
    #[default]
    Gpu,
    /// Resample on the CPU at background priority (no VRAM beyond the copy).
    Cpu,
}

/// When a backgrounded window's RAM source is evicted (re-decoded on return).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictPolicy {
    /// Never evict the full-res source from RAM.
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

/// When (and how) a backgrounded window evicts a decoded source from RAM, which
/// is re-decoded from disk on return. Shared by the still pipeline, where it is
/// the last of three stages, and the animated one, where it is the *only* stage:
/// an animation has no governed VRAM tier, so demote/drop do not exist for
/// it (this type is the whole animated decay config).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvictConfig {
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

impl Default for EvictConfig {
    /// Conservative fallback for a deleted key: never evict. The real per-state
    /// defaults are set in [`ResourceConfig::default`].
    fn default() -> Self {
        Self {
            evict_ram: EvictPolicy::Never,
            evict_ram_min: Duration::from_secs(30),
            evict_ram_max: Duration::from_secs(600),
            max_decode_latency: Duration::from_millis(200),
        }
    }
}

impl EvictConfig {
    /// The RAM-eviction delay for an image given its decode time, or `None` to
    /// never evict. Dynamic mode interpolates linearly between `evict_ram_min`
    /// (instant decode) and `evict_ram_max` (at the latency ceiling). An image
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

/// The decay pipeline a backgrounded window runs for a still: full-res VRAM, then
/// (after each timer) demote to view-res, drop the VRAM, and evict the RAM source.
/// A `None` timer skips that stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DecayPipeline {
    /// Demote the on-screen image full-res -> view-res after this delay.
    #[serde(with = "humantime_opt")]
    pub demote_vram_after: Option<Duration>,
    /// Drop the on-screen image's VRAM entirely after this delay.
    #[serde(with = "humantime_opt")]
    pub drop_vram_after: Option<Duration>,
    /// The RAM-eviction stage, flattened so its keys sit alongside the demote and
    /// drop timers in `[resource.*.still]`.
    #[serde(flatten)]
    pub evict: EvictConfig,
}

/// Decay timing for an open video. A video has no governed VRAM tier to demote or
/// drop. The heavy resource is its whole decode session (the decode threads, the
/// hardware decoder, the audio sink, and the GPU plane textures). After this delay a
/// backgrounded window releases that session, freezing the last frame on screen, and
/// re-opens it at the saved position when the window returns. A `None` timer keeps the
/// session alive (a minimized window still pauses it via [`MinimizedConfig::pause_video`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VideoDecay {
    /// Release the decode session this long after the window is backgrounded.
    #[serde(with = "humantime_opt")]
    pub evict_session_after: Option<Duration>,
}

/// The event that starts a backgrounded window's prefetch shedding.
/// `Immediately` counts from entering the state. The other anchors count from
/// the on-screen image's decay reaching that stage. A skipped anchor falls
/// through to the next stage that runs (`Demote` sheds with the drop stage
/// when demote is `"never"`, or with an animation's evict or a video's session
/// release). If no stage at or after the anchor runs, the prefetch is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrefetchDropAnchor {
    /// Count from the moment the window enters the state.
    Immediately,
    /// Count from the on-screen image's demote stage running.
    #[default]
    Demote,
    /// Count from the on-screen image's VRAM-drop stage running.
    Drop,
    /// Count from the on-screen image's RAM-evict stage running (for a video,
    /// its session release).
    Evict,
}

/// When a backgrounded window sheds its prefetched neighbors, and how fast.
/// Shedding walks inward ring by ring from the furthest neighbors: each step
/// releases the most distant ring (up to one image per side), so the
/// neighbors most likely to be shown next are the last to go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefetchDecay {
    /// The event the shedding counts from.
    pub drop_on: PrefetchDropAnchor,
    /// How long after the event the first ring is released.
    #[serde(with = "humantime_dur")]
    pub drop_after: Duration,
    /// The pause between one ring and the next. Zero sheds everything at once.
    #[serde(with = "humantime_dur")]
    pub drop_interval: Duration,
}

impl Default for PrefetchDecay {
    /// Conservative fallback for a deleted key: shed with the demote stage, all
    /// at once. The real per-state defaults are set in [`ResourceConfig::default`].
    fn default() -> Self {
        Self {
            drop_on: PrefetchDropAnchor::Demote,
            drop_after: Duration::ZERO,
            drop_interval: Duration::ZERO,
        }
    }
}

/// A backgrounded state's decay timers, split by media kind. Stills, animations, and
/// video decay independently: only the evict stage applies to an animation (it has no
/// governed VRAM tier), and re-decoding one is costly, so animations are usually kept
/// in RAM longer than stills. A video has only the session-release timer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StateDecay {
    /// Decay timers for a still image.
    pub still: DecayPipeline,
    /// Eviction timing for an animation (GIF, APNG, animated WebP). Only eviction
    /// applies, so demote/drop are not configurable for it.
    pub animated: EvictConfig,
    /// When to release an open video's decode session.
    pub video: VideoDecay,
    /// When the prefetched neighbors are shed, independent of the media kind
    /// on screen.
    pub prefetch: PrefetchDecay,
}

/// The minimized state's decay timers plus its video-pause toggle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MinimizedConfig {
    /// Pause an open video while the window is minimized.
    pub pause_video: bool,
    /// Decay timers for a still image.
    pub still: DecayPipeline,
    /// Eviction timing for an animation.
    pub animated: EvictConfig,
    /// When to release an open video's decode session.
    pub video: VideoDecay,
    /// When the prefetched neighbors are shed, independent of the media kind
    /// on screen.
    pub prefetch: PrefetchDecay,
}

impl Default for MinimizedConfig {
    fn default() -> Self {
        Self {
            pause_video: true,
            still: DecayPipeline::default(),
            animated: EvictConfig::default(),
            video: VideoDecay::default(),
            prefetch: PrefetchDecay::default(),
        }
    }
}

/// When the process empties its working set back to the OS (Windows only).
/// `EmptyWorkingSet` is process-global, so it fires only once the whole app is in
/// the background by the chosen measure, never per window.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkingSetTrim {
    /// Never trim.
    #[default]
    Never,
    /// Trim once every window has lost focus (the app is not the foreground app).
    AllUnfocused,
    /// Trim once every window is minimized (the app is fully hidden).
    AllMinimized,
}

/// Working-set trimming for the idle process (Windows only). When the chosen
/// condition holds across every window without interruption for `trim_after`, the
/// process empties its working set, returning resident pages to the OS so the
/// background footprint drops. The pages fault back in on return, so this trades a
/// little restore latency for a smaller idle footprint.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkingSetConfig {
    /// The background condition that arms the trim.
    pub trim_when: WorkingSetTrim,
    /// How long the condition must hold before the trim fires.
    #[serde(with = "humantime_dur")]
    pub trim_after: Duration,
}

#[cfg(target_os = "windows")]
impl Default for WorkingSetConfig {
    fn default() -> Self {
        Self {
            trim_when: WorkingSetTrim::Never,
            trim_after: Duration::from_secs(10),
        }
    }
}

/// Ceiling for one image's decoded RGBA bytes: a fraction of the machine's
/// RAM (`"50%"`) or an absolute size (`"2GB"`, `"500MB"`). An image whose
/// decode would exceed it opens downscaled to fit instead of failing.
/// Reference: a 1-gigapixel image decodes to 4 GB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamBudget {
    /// Percentage of total system RAM, 1 to 100.
    Percent(u8),
    /// Absolute byte size.
    Bytes(u64),
}

impl Default for RamBudget {
    fn default() -> Self {
        RamBudget::Percent(50)
    }
}

/// Byte scales for parsing and display. Decimal units are powers of 1000,
/// binary (`KiB` family) powers of 1024, per IEC 80000-13.
/// https://en.wikipedia.org/wiki/ISO/IEC_80000#Information_science_and_technology
const BYTE_UNITS: &[(&str, u64)] = &[
    ("TiB", 1 << 40),
    ("TB", 1_000_000_000_000),
    ("GiB", 1 << 30),
    ("GB", 1_000_000_000),
    ("MiB", 1 << 20),
    ("MB", 1_000_000),
    ("KiB", 1 << 10),
    ("KB", 1_000),
    ("B", 1),
];

impl RamBudget {
    /// The budget in bytes for a machine with `total_ram` bytes. An unknown
    /// total (zero) turns a percentage into no limit rather than clamping
    /// every image to nothing.
    pub fn resolve(self, total_ram: u64) -> u64 {
        match self {
            RamBudget::Percent(_) if total_ram == 0 => u64::MAX,
            RamBudget::Percent(p) => ((total_ram as u128 * p as u128) / 100) as u64,
            RamBudget::Bytes(b) => b,
        }
    }
}

impl std::fmt::Display for RamBudget {
    /// Lossless: bytes print in the largest unit that divides them exactly,
    /// so a hand-written value survives every save/load cycle unchanged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RamBudget::Percent(p) => write!(f, "{p}%"),
            RamBudget::Bytes(n) => {
                let (suffix, scale) = BYTE_UNITS
                    .iter()
                    .find(|(_, scale)| n % scale == 0 && n / scale > 0)
                    .unwrap_or(&("B", 1));
                write!(f, "{}{suffix}", n / scale)
            }
        }
    }
}

impl std::str::FromStr for RamBudget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(percent) = s.strip_suffix('%') {
            let p: u8 = percent
                .trim()
                .parse()
                .map_err(|_| format!("invalid percentage: {s:?}"))?;
            if p == 0 || p > 100 {
                return Err(format!("percentage out of range 1-100: {s:?}"));
            }
            return Ok(RamBudget::Percent(p));
        }
        let unit_start = s
            .find(|c: char| c.is_ascii_alphabetic())
            .ok_or_else(|| format!("missing unit (B, KB, MB, GB, TB or %): {s:?}"))?;
        let (number, unit) = s.split_at(unit_start);
        let (_, scale) = BYTE_UNITS
            .iter()
            .find(|(suffix, _)| suffix.eq_ignore_ascii_case(unit))
            .ok_or_else(|| format!("unknown unit {unit:?}"))?;
        let value: f64 = number
            .trim()
            .parse()
            .map_err(|_| format!("invalid number: {s:?}"))?;
        if value.is_nan() || value <= 0.0 {
            return Err(format!("size must be positive: {s:?}"));
        }
        Ok(RamBudget::Bytes((value * *scale as f64).round() as u64))
    }
}

impl Serialize for RamBudget {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RamBudget {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Total physical RAM in bytes, queried once. Zero when the query fails,
/// which [`RamBudget::resolve`] treats as no limit.
pub fn total_system_ram() -> u64 {
    static TOTAL: LazyLock<u64> = LazyLock::new(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        system.total_memory()
    });
    *TOTAL
}

/// Advanced memory/VRAM resource model. The defaults are scryglass's opinion;
/// every field is tunable in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// What resolution a focused window's prefetch neighbors upload at.
    pub prefetch_vram: PrefetchVram,
    /// Where a prefetch neighbor's view-res copy is produced (GPU or CPU).
    pub prefetch_scaler: PrefetchScaler,
    /// How many prefetch neighbors decode at once: `"auto"` or a count.
    pub prefetch_parallelism: PrefetchParallelism,
    /// Ceiling for one image's decoded bytes: `"50%"` of RAM or `"2GB"`.
    /// Kept before the sub-tables so the TOML serializer accepts it.
    pub large_image_ram_budget: RamBudget,
    /// Decay timers for an unfocused window, by media kind.
    pub unfocused: StateDecay,
    /// Decay timers for a minimized window, by media kind.
    pub minimized: MinimizedConfig,
    /// Working-set trimming for the idle process (Windows only).
    #[cfg(target_os = "windows")]
    pub working_set: WorkingSetConfig,
}

/// One backgrounded state's decay tree, viewed uniformly whether the window is
/// unfocused or minimized. The decay engine reads the four subtrees through this
/// view, so the two states are interchangeable values of one shape. Only the
/// minimized-specific extras (`pause_video`) sit outside it.
#[derive(Clone, Copy)]
pub struct StateDecayRef<'a> {
    pub still: &'a DecayPipeline,
    pub animated: &'a EvictConfig,
    pub video: &'a VideoDecay,
    pub prefetch: &'a PrefetchDecay,
}

impl ResourceConfig {
    /// The decay tree governing a backgrounded window in the given state.
    pub fn for_state(&self, minimized: bool) -> StateDecayRef<'_> {
        if minimized {
            StateDecayRef {
                still: &self.minimized.still,
                animated: &self.minimized.animated,
                video: &self.minimized.video,
                prefetch: &self.minimized.prefetch,
            }
        } else {
            StateDecayRef {
                still: &self.unfocused.still,
                animated: &self.unfocused.animated,
                video: &self.unfocused.video,
                prefetch: &self.unfocused.prefetch,
            }
        }
    }
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            prefetch_vram: PrefetchVram::ViewRes,
            prefetch_scaler: PrefetchScaler::default(),
            prefetch_parallelism: PrefetchParallelism::default(),
            large_image_ram_budget: RamBudget::default(),
            unfocused: StateDecay {
                still: DecayPipeline {
                    demote_vram_after: Some(Duration::from_secs(15)),
                    drop_vram_after: None,
                    evict: EvictConfig {
                        evict_ram: EvictPolicy::Dynamic,
                        evict_ram_min: Duration::from_secs(30),
                        evict_ram_max: Duration::from_secs(600),
                        max_decode_latency: Duration::from_millis(200),
                    },
                },
                // Unfocused animations stay in RAM: re-decoding every frame is
                // costly and a refocus should be instant.
                animated: EvictConfig::default(),
                // An unfocused video is still visible, so keep it playing: only a
                // hidden (minimized) window releases the decode session.
                video: VideoDecay {
                    evict_session_after: None,
                },
                // Unfocused prefetch sheds with the demote stage, all rings at
                // once: the window is still visible, so its look-ahead keeps
                // the same 15 s grace the on-screen image gets.
                prefetch: PrefetchDecay {
                    drop_on: PrefetchDropAnchor::Demote,
                    drop_after: Duration::ZERO,
                    drop_interval: Duration::ZERO,
                },
            },
            minimized: MinimizedConfig {
                pause_video: true,
                still: DecayPipeline {
                    demote_vram_after: None,
                    drop_vram_after: Some(Duration::ZERO),
                    evict: EvictConfig {
                        evict_ram: EvictPolicy::Dynamic,
                        evict_ram_min: Duration::from_secs(15),
                        evict_ram_max: Duration::from_secs(300),
                        max_decode_latency: Duration::from_millis(200),
                    },
                },
                // Minimized animations free their RAM frames after a delay.
                // A restore re-decodes them.
                animated: EvictConfig {
                    evict_ram: EvictPolicy::Fixed(Duration::from_secs(30)),
                    ..EvictConfig::default()
                },
                // A minimized window is hidden, so release the whole decode session
                // after a short grace. A restore re-opens it at the saved position.
                video: VideoDecay {
                    evict_session_after: Some(Duration::from_secs(5)),
                },
                // A minimized window sheds its look-ahead on a timer of its own
                // (its still pipeline has no demote stage to anchor to): a
                // quick alt-tab back keeps the whole deck, a parked window
                // walks it in from the furthest ring.
                prefetch: PrefetchDecay {
                    drop_on: PrefetchDropAnchor::Immediately,
                    drop_after: Duration::from_secs(15),
                    drop_interval: Duration::from_secs(5),
                },
            },
            // Off by default: the trim is an aggressive, Windows-only knob whose
            // re-fault on return the user should opt into.
            #[cfg(target_os = "windows")]
            working_set: WorkingSetConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn for_state_borrows_the_matching_subtree() {
        let res = ResourceConfig::default();
        let min = res.for_state(true);
        assert!(std::ptr::eq(min.still, &res.minimized.still));
        assert!(std::ptr::eq(min.prefetch, &res.minimized.prefetch));
        let unf = res.for_state(false);
        assert!(std::ptr::eq(unf.animated, &res.unfocused.animated));
        assert!(std::ptr::eq(unf.video, &res.unfocused.video));
    }

    #[test]
    fn prefetch_decay_defaults_differ_by_state() {
        let res = AppConfig::default().resource;
        // Unfocused: the look-ahead sheds with the demote stage, all at once.
        assert_eq!(res.unfocused.prefetch.drop_on, PrefetchDropAnchor::Demote);
        assert_eq!(res.unfocused.prefetch.drop_after, Duration::ZERO);
        assert_eq!(res.unfocused.prefetch.drop_interval, Duration::ZERO);
        // Minimized: its still pipeline has no demote stage, so the shedding
        // runs on its own timer, ring by ring.
        assert_eq!(
            res.minimized.prefetch.drop_on,
            PrefetchDropAnchor::Immediately
        );
        assert_eq!(res.minimized.prefetch.drop_after, Duration::from_secs(15));
        assert_eq!(res.minimized.prefetch.drop_interval, Duration::from_secs(5));
    }

    #[test]
    fn prefetch_decay_parses_from_toml() {
        let cfg = AppConfig::from_toml(
            "[resource.minimized.prefetch]\n\
             drop_on = \"evict\"\n\
             drop_after = \"30s\"\n\
             drop_interval = \"2s\"\n",
        );
        assert_eq!(
            cfg.resource.minimized.prefetch.drop_on,
            PrefetchDropAnchor::Evict
        );
        assert_eq!(
            cfg.resource.minimized.prefetch.drop_after,
            Duration::from_secs(30)
        );
        assert_eq!(
            cfg.resource.minimized.prefetch.drop_interval,
            Duration::from_secs(2)
        );
        // The other state keeps its own default.
        assert_eq!(
            cfg.resource.unfocused.prefetch.drop_on,
            PrefetchDropAnchor::Demote
        );
    }

    #[test]
    fn prefetch_parallelism_parses_auto_and_counts() {
        assert_eq!("auto".parse(), Ok(PrefetchParallelism::Auto));
        assert_eq!("Auto".parse(), Ok(PrefetchParallelism::Auto));
        assert_eq!(" 4 ".parse(), Ok(PrefetchParallelism::Fixed(4)));
        assert!("four".parse::<PrefetchParallelism>().is_err());
        assert!("-1".parse::<PrefetchParallelism>().is_err());
    }

    #[test]
    fn prefetch_parallelism_resolves_and_displays() {
        assert!(PrefetchParallelism::Auto.resolve() >= 2);
        assert_eq!(PrefetchParallelism::Fixed(0).resolve(), 1);
        assert_eq!(PrefetchParallelism::Fixed(3).resolve(), 3);
        assert_eq!(PrefetchParallelism::Auto.to_string(), "auto");
        assert_eq!(PrefetchParallelism::Fixed(3).to_string(), "3");
    }

    #[test]
    fn ram_budget_parses_percentages_and_sizes() {
        let parse = |s: &str| s.parse::<RamBudget>();
        assert_eq!(parse("50%"), Ok(RamBudget::Percent(50)));
        assert_eq!(parse(" 100 % "), Ok(RamBudget::Percent(100)));
        assert_eq!(parse("2GB"), Ok(RamBudget::Bytes(2_000_000_000)));
        assert_eq!(parse("500mb"), Ok(RamBudget::Bytes(500_000_000)));
        assert_eq!(parse("1.5 GiB"), Ok(RamBudget::Bytes(1_610_612_736)));
        assert_eq!(parse("123456B"), Ok(RamBudget::Bytes(123_456)));
    }

    #[test]
    fn ram_budget_rejects_nonsense() {
        for bad in ["", "5", "0%", "101%", "-5%", "2XB", "GB", "0B", "-1GB"] {
            assert!(
                bad.parse::<RamBudget>().is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn ram_budget_display_is_lossless_and_readable() {
        let round = |b: RamBudget| b.to_string().parse::<RamBudget>().unwrap();
        for budget in [
            RamBudget::Percent(50),
            RamBudget::Bytes(2_000_000_000),
            RamBudget::Bytes(1_610_612_736),
            RamBudget::Bytes(123_457),
        ] {
            assert_eq!(round(budget), budget);
        }
        assert_eq!(RamBudget::Percent(50).to_string(), "50%");
        assert_eq!(RamBudget::Bytes(2_000_000_000).to_string(), "2GB");
        assert_eq!(RamBudget::Bytes(1 << 30).to_string(), "1GiB");
        assert_eq!(RamBudget::Bytes(123_457).to_string(), "123457B");
    }

    #[test]
    fn ram_budget_resolves_against_total_ram() {
        const GIB: u64 = 1 << 30;
        assert_eq!(RamBudget::Percent(50).resolve(16 * GIB), 8 * GIB);
        assert_eq!(RamBudget::Percent(100).resolve(16 * GIB), 16 * GIB);
        assert_eq!(RamBudget::Bytes(123).resolve(16 * GIB), 123);
        // An unknown total must not clamp everything to nothing.
        assert_eq!(RamBudget::Percent(50).resolve(0), u64::MAX);
        assert_eq!(RamBudget::Bytes(123).resolve(0), 123);
    }

    #[test]
    fn total_system_ram_looks_like_bytes() {
        // Guards against the query reporting kilobytes: any machine that can
        // run the test suite has at least a quarter GiB.
        assert!(total_system_ram() >= 256 * 1024 * 1024);
    }

    #[test]
    fn ram_budget_default_is_half_of_ram() {
        assert_eq!(
            ResourceConfig::default().large_image_ram_budget,
            RamBudget::Percent(50)
        );
    }

    #[test]
    fn timers_parse_human_durations_and_never() {
        let cfg = AppConfig::from_toml(
            "[resource.unfocused.still]\ndemote_vram_after = \"500ms\"\ndrop_vram_after = \"never\"\n",
        );
        assert_eq!(
            cfg.resource.unfocused.still.demote_vram_after,
            Some(Duration::from_millis(500))
        );
        assert_eq!(cfg.resource.unfocused.still.drop_vram_after, None);
    }

    #[test]
    fn video_session_eviction_parses_and_defaults() {
        // Default: an unfocused (visible) video keeps its session, a minimized
        // (hidden) one releases it after a short grace.
        let d = ResourceConfig::default();
        assert_eq!(d.unfocused.video.evict_session_after, None);
        assert_eq!(
            d.minimized.video.evict_session_after,
            Some(Duration::from_secs(5))
        );
        // Parses a duration and "never".
        let cfg = AppConfig::from_toml(
            "[resource.unfocused.video]\nevict_session_after = \"10s\"\n\
             [resource.minimized.video]\nevict_session_after = \"never\"\n",
        );
        assert_eq!(
            cfg.resource.unfocused.video.evict_session_after,
            Some(Duration::from_secs(10))
        );
        assert_eq!(cfg.resource.minimized.video.evict_session_after, None);
    }

    #[test]
    fn evict_policy_parses_never_dynamic_and_a_duration() {
        let parse = |s: &str| {
            AppConfig::from_toml(&format!(
                "[resource.unfocused.still]\nevict_ram = \"{s}\"\n"
            ))
            .resource
            .unfocused
            .still
            .evict
            .evict_ram
        };
        assert_eq!(parse("never"), EvictPolicy::Never);
        assert_eq!(parse("dynamic"), EvictPolicy::Dynamic);
        assert_eq!(parse("2m"), EvictPolicy::Fixed(Duration::from_secs(120)));
    }

    #[test]
    fn evict_delay_never_and_fixed() {
        let mut p = EvictConfig::default();
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
        let p = EvictConfig {
            evict_ram: EvictPolicy::Dynamic,
            evict_ram_min: Duration::from_secs(30),
            evict_ram_max: Duration::from_secs(630),
            max_decode_latency: Duration::from_millis(200),
        };
        // Instant decode -> min, halfway -> midpoint, at/over ceiling -> never.
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
}
