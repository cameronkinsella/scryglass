//! UI-facing configuration enums: theme, sort order, zoom mode, and the
//! downscale kernel.

use serde::{Deserialize, Serialize};

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

/// Kernel used to shrink a still or animation to fit. All fix the aliasing a plain
/// bilinear tap leaves on heavy minification. They differ only in sharpness and
/// whether they ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownscaleKernel {
    /// One bilinear tap (fastest, softens and aliases when shrunk past ~2x).
    Bilinear,
    /// Balanced cubic that never rings: the safe default for mixed content.
    #[default]
    Mitchell,
    /// Sharper cubic with a small overshoot (mild ringing).
    CatmullRom,
    /// Sharpest, with visible ringing. Best on clean photographic detail.
    Lanczos3,
}

impl DownscaleKernel {
    /// The shader's kernel selector (`flags.y`) and Mitchell-Netravali `(B, C)`.
    /// The cubics share one selector and differ only by `(B, C)`; Lanczos ignores it.
    pub fn shader_params(self) -> (u32, [f32; 2]) {
        match self {
            DownscaleKernel::Bilinear => (0, [0.0, 0.0]),
            DownscaleKernel::Mitchell => (1, [1.0 / 3.0, 1.0 / 3.0]),
            DownscaleKernel::CatmullRom => (1, [0.0, 0.5]),
            DownscaleKernel::Lanczos3 => (2, [0.0, 0.0]),
        }
    }

    /// Round-trip as a small integer, so the live kernel can ride an atomic from
    /// the display draw to the off-thread view-res render.
    pub fn to_u8(self) -> u8 {
        match self {
            DownscaleKernel::Bilinear => 0,
            DownscaleKernel::Mitchell => 1,
            DownscaleKernel::CatmullRom => 2,
            DownscaleKernel::Lanczos3 => 3,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => DownscaleKernel::Bilinear,
            2 => DownscaleKernel::CatmullRom,
            3 => DownscaleKernel::Lanczos3,
            _ => DownscaleKernel::Mitchell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DisplayConfig, StandardConfig};

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(
            AppConfig::default().standard.display.theme,
            ThemeChoice::Dark
        );
    }

    #[test]
    fn old_natural_name_sort_value_still_parses() {
        let cfg = AppConfig::from_toml("[standard.display]\nsort_key = \"NaturalName\"\n");
        assert_eq!(cfg.standard.display.sort_key, SortKey::Name);
    }

    #[test]
    fn zoom_mode_serializes_as_readable_name() {
        let cfg = AppConfig {
            standard: StandardConfig {
                display: DisplayConfig {
                    zoom_mode: ZoomMode::LockZoomRatio,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.to_toml().contains("LockZoomRatio"));
    }
}
