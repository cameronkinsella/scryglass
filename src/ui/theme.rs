//! Semantic color tokens and the application themes.
//!
//! All chrome colors live here. Widget modules must not hardcode colors.
//! The dark palette is near-black and neutral (no blue cast) so photos pop;
//! the light palette mirrors it for bright environments.

use std::sync::LazyLock;

use iced::theme::Palette;
use iced::widget::button;
use iced::widget::container;
use iced::widget::text;
use iced::{Background, Border, Color, Shadow, Theme, Vector};

/// Semantic colors used across the UI.
pub struct Tokens {
    /// Window and image-viewport background.
    pub bg_base: Color,
    /// Toolbar, footer, and other chrome bars.
    pub bg_surface: Color,
    /// Menus, dialogs, and floating panels.
    pub bg_elevated: Color,
    /// Primary text.
    pub text_primary: Color,
    /// De-emphasized text (hints, prompts, labels).
    pub text_secondary: Color,
    /// Selection, highlights, slider fill.
    pub accent: Color,
    /// Destructive actions and errors.
    pub danger: Color,
    /// Panel outlines and separators.
    pub border: Color,
}

const fn rgb8(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

const fn with_alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub const DARK: Tokens = Tokens {
    bg_base: rgb8(0x0E, 0x0E, 0x10),
    bg_surface: rgb8(0x18, 0x18, 0x1B),
    bg_elevated: rgb8(0x22, 0x22, 0x26),
    text_primary: rgb8(0xE6, 0xE6, 0xE9),
    text_secondary: rgb8(0x98, 0x98, 0x9F),
    accent: rgb8(0x6C, 0xA0, 0xDC),
    danger: rgb8(0xD9, 0x53, 0x4F),
    border: rgb8(0x2E, 0x2E, 0x33),
};

pub const LIGHT: Tokens = Tokens {
    bg_base: rgb8(0xFA, 0xFA, 0xFB),
    bg_surface: rgb8(0xF0, 0xF0, 0xF2),
    bg_elevated: rgb8(0xFF, 0xFF, 0xFF),
    text_primary: rgb8(0x1A, 0x1A, 0x1E),
    text_secondary: rgb8(0x5A, 0x5A, 0x64),
    accent: rgb8(0x3B, 0x82, 0xC4),
    danger: rgb8(0xC7, 0x3E, 0x3A),
    border: rgb8(0xD4, 0xD4, 0xDC),
};

static DARK_THEME: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "Scryglass Dark",
        Palette {
            background: DARK.bg_base,
            text: DARK.text_primary,
            primary: DARK.accent,
            success: rgb8(0x3F, 0xB6, 0x8B),
            warning: rgb8(0xD9, 0xA0, 0x3F),
            danger: DARK.danger,
        },
    )
});

static LIGHT_THEME: LazyLock<Theme> = LazyLock::new(|| {
    Theme::custom(
        "Scryglass Light",
        Palette {
            background: LIGHT.bg_base,
            text: LIGHT.text_primary,
            primary: LIGHT.accent,
            success: rgb8(0x2E, 0x8B, 0x6A),
            warning: rgb8(0xB0, 0x7D, 0x2B),
            danger: LIGHT.danger,
        },
    )
});

/// The dark application theme (default).
pub fn dark() -> Theme {
    DARK_THEME.clone()
}

/// The light application theme.
pub fn light() -> Theme {
    LIGHT_THEME.clone()
}

/// Tokens for the active theme.
pub fn tokens(theme: &Theme) -> &'static Tokens {
    if theme.extended_palette().is_dark {
        &DARK
    } else {
        &LIGHT
    }
}

// ---------------------------------------------------------------------------
// Shared style functions
// ---------------------------------------------------------------------------

/// Chrome bar background (toolbar, footer).
pub fn surface(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(tokens(theme).bg_surface)),
        ..container::Style::default()
    }
}

/// Floating panel (dropdown menus, context menu, dialogs).
pub fn panel(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(Background::Color(t.bg_elevated)),
        border: Border {
            color: t.border,
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: Shadow {
            color: with_alpha(Color::BLACK, 0.4),
            offset: Vector::new(0.0, 3.0),
            blur_radius: 12.0,
        },
        ..container::Style::default()
    }
}

/// De-emphasized text (prompts, hints).
pub fn secondary_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(tokens(theme).text_secondary),
    }
}

/// Menu selection checkmark: accent when selected, invisible otherwise
/// (keeps label alignment identical across items).
pub fn check_indicator(selected: bool) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style {
        color: Some(if selected {
            tokens(theme).accent
        } else {
            Color::TRANSPARENT
        }),
    }
}

/// Info toast card.
pub fn toast_info(theme: &Theme) -> container::Style {
    panel(theme)
}

/// Error toast card, a panel with a danger accent border.
pub fn toast_error(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        border: Border {
            color: t.danger,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..panel(theme)
    }
}

/// Menu/context item: flat, full-width, subtle accent wash on hover.
pub fn menu_item(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let background = match status {
        button::Status::Hovered => Some(Background::Color(with_alpha(t.accent, 0.22))),
        button::Status::Pressed => Some(Background::Color(with_alpha(t.accent, 0.32))),
        _ => None,
    };
    button::Style {
        background,
        text_color: t.text_primary,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Menu-bar tab: transparent by default, subtle highlight on hover.
pub fn menu_tab(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let background = match status {
        button::Status::Hovered => Some(Background::Color(with_alpha(t.text_primary, 0.08))),
        button::Status::Pressed => Some(Background::Color(with_alpha(t.text_primary, 0.14))),
        _ => None,
    };
    button::Style {
        background,
        text_color: t.text_primary,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Menu-bar tab whose dropdown is currently open.
pub fn menu_tab_active(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let alpha = match status {
        button::Status::Pressed => 0.18,
        _ => 0.12,
    };
    button::Style {
        background: Some(Background::Color(with_alpha(t.text_primary, alpha))),
        text_color: t.text_primary,
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Empty filmstrip cell awaiting its thumbnail.
pub fn thumb_placeholder(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(Background::Color(with_alpha(t.text_primary, 0.06))),
        border: Border {
            radius: 3.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Filmstrip thumbnail for the current image, accent border.
pub fn thumb_current(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let bg_alpha = match status {
        button::Status::Hovered | button::Status::Pressed => 0.30,
        _ => 0.12,
    };
    button::Style {
        background: Some(Background::Color(with_alpha(t.accent, bg_alpha))),
        border: Border {
            color: t.accent,
            width: 3.0,
            radius: 4.0.into(),
        },
        ..button::Style::default()
    }
}

/// Filmstrip thumbnail for other images, border appears on hover.
pub fn thumb(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    match status {
        button::Status::Hovered => button::Style {
            background: Some(Background::Color(with_alpha(t.text_primary, 0.10))),
            border: Border {
                color: with_alpha(t.text_primary, 0.55),
                width: 3.0,
                radius: 4.0.into(),
            },
            ..button::Style::default()
        },
        button::Status::Pressed => button::Style {
            background: Some(Background::Color(with_alpha(t.text_primary, 0.16))),
            border: Border {
                color: t.text_primary,
                width: 3.0,
                radius: 4.0.into(),
            },
            ..button::Style::default()
        },
        _ => button::Style {
            background: None,
            border: Border {
                color: Color::TRANSPARENT,
                width: 3.0,
                radius: 4.0.into(),
            },
            ..button::Style::default()
        },
    }
}
