//! Launch-time configuration: the present mode chosen before the first
//! window's surface exists.

use serde::{Deserialize, Serialize};

/// How rendered frames are handed to the display. Fixed for the life of the
/// process when the first window's surface is created, hence `[startup]`.
/// Applied by handing iced the matching `ICED_PRESENT_MODE` value in `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentMode {
    /// Let iced and wgpu pick: vsync through the first supported synced mode.
    Auto,
    /// Refresh-synced without queue blocking: no tearing, and live window
    /// resizes stay clean where the blocking modes flicker at the edges
    /// (https://github.com/gfx-rs/wgpu/issues/5374). Not universal, so
    /// startup probes the driver and falls back to `Auto` when missing.
    Mailbox,
    /// The classic vsync queue. The one mode every driver must support
    /// (https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html).
    Fifo,
    /// Vsync that tears instead of stalling when a frame misses the blank.
    FifoRelaxed,
    /// No sync at all: lowest latency, tears freely.
    Immediate,
    /// Vsync off with graceful fallback, so it works on any driver.
    NoVsync,
}

impl Default for PresentMode {
    /// Windows favors mailbox: the blocking synced modes there flicker during
    /// live window resizes (wgpu issue 5374) and mailbox keeps playback
    /// refresh-synced and tear-free without them. Elsewhere iced's default
    /// stands.
    fn default() -> Self {
        if cfg!(target_os = "windows") {
            PresentMode::Mailbox
        } else {
            PresentMode::Auto
        }
    }
}

impl PresentMode {
    /// The `ICED_PRESENT_MODE` value iced parses for this mode, or `None` for
    /// `Auto`, which leaves iced's built-in default in force. The strings must
    /// match `present_mode_from_env` in `iced_wgpu` (settings.rs).
    pub fn env_value(self) -> Option<&'static str> {
        match self {
            PresentMode::Auto => None,
            PresentMode::Mailbox => Some("mailbox"),
            PresentMode::Fifo => Some("fifo"),
            PresentMode::FifoRelaxed => Some("fifo_relaxed"),
            PresentMode::Immediate => Some("immediate"),
            PresentMode::NoVsync => Some("no_vsync"),
        }
    }

    /// Whether a driver may lack this mode. wgpu rejects an unsupported
    /// explicit mode when the surface is configured and iced treats that as
    /// fatal, so these are verified against the driver before the UI boots.
    /// `Fifo` is spec-guaranteed and the auto modes carry fallback chains.
    // Compiled for tests everywhere. Only the Windows startup path calls it.
    #[cfg(any(target_os = "windows", test))]
    pub fn needs_probe(self) -> bool {
        matches!(
            self,
            PresentMode::Mailbox | PresentMode::FifoRelaxed | PresentMode::Immediate
        )
    }

    /// The mode to run with, given the driver's supported modes from the
    /// startup probe (`None` when the probe failed). A probeable mode the
    /// driver lacks falls back to `Auto` instead of crashing the first window.
    #[cfg(any(target_os = "windows", test))]
    pub fn resolve(self, supported: Option<&[PresentMode]>) -> PresentMode {
        if !self.needs_probe() {
            return self;
        }
        match supported {
            Some(modes) if modes.contains(&self) => self,
            _ => PresentMode::Auto,
        }
    }
}

/// Settings applied once, before the first window exists. Edits take effect
/// on the next full restart: every scryglass window closed, then a fresh
/// launch. Opening a file while scryglass is running joins the existing
/// process, which keeps its startup settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupConfig {
    /// How rendered frames are handed to the display.
    pub present_mode: PresentMode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn present_mode_default_is_mailbox_on_windows_only() {
        let expected = if cfg!(target_os = "windows") {
            PresentMode::Mailbox
        } else {
            PresentMode::Auto
        };
        assert_eq!(AppConfig::default().startup.present_mode, expected);
    }

    #[test]
    fn present_mode_env_values_match_what_iced_parses() {
        // The accepted strings of present_mode_from_env in iced_wgpu 0.14.
        assert_eq!(PresentMode::Auto.env_value(), None);
        assert_eq!(PresentMode::Mailbox.env_value(), Some("mailbox"));
        assert_eq!(PresentMode::Fifo.env_value(), Some("fifo"));
        assert_eq!(PresentMode::FifoRelaxed.env_value(), Some("fifo_relaxed"));
        assert_eq!(PresentMode::Immediate.env_value(), Some("immediate"));
        assert_eq!(PresentMode::NoVsync.env_value(), Some("no_vsync"));
    }

    #[test]
    fn guaranteed_present_modes_skip_the_probe() {
        for mode in [PresentMode::Auto, PresentMode::Fifo, PresentMode::NoVsync] {
            assert!(!mode.needs_probe());
            assert_eq!(mode.resolve(None), mode);
            assert_eq!(mode.resolve(Some(&[])), mode);
        }
    }

    #[test]
    fn probeable_present_modes_fall_back_to_auto_unless_supported() {
        for mode in [
            PresentMode::Mailbox,
            PresentMode::FifoRelaxed,
            PresentMode::Immediate,
        ] {
            assert!(mode.needs_probe());
            assert_eq!(mode.resolve(Some(&[mode, PresentMode::Fifo])), mode);
            assert_eq!(mode.resolve(Some(&[PresentMode::Fifo])), PresentMode::Auto);
            assert_eq!(mode.resolve(None), PresentMode::Auto);
        }
    }

    #[test]
    fn present_mode_parses_kebab_case() {
        let cfg = AppConfig::from_toml("[startup]\npresent_mode = \"fifo-relaxed\"\n");
        assert_eq!(cfg.startup.present_mode, PresentMode::FifoRelaxed);
    }
}
