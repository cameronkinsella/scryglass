//! Toolbar widget: "File" dropdown menu with Open, Close, and Quit actions.
//!
//! Renders a menu-bar style row. Clicking "File" toggles a dropdown that
//! floats over the content below. Menu items are flat, borderless buttons
//! with a subtle hover highlight, matching the look of a native menu bar.

use iced::widget::button::{self, Status, Style};
use iced::widget::{column, container, row, text};
use iced::{Background, Border, Color, Element, Length, Padding, Theme};

use crate::app::Message;

// ---------------------------------------------------------------------------
// Custom button styles
// ---------------------------------------------------------------------------

/// Menu-bar tab style: transparent by default, subtle highlight on hover.
fn menu_tab_style(theme: &Theme, status: Status) -> Style {
    let palette = theme.extended_palette();
    match status {
        Status::Active | Status::Disabled => Style {
            background: None,
            text_color: palette.background.base.text,
            border: Border::default(),
            ..Style::default()
        },
        Status::Hovered => Style {
            background: Some(Background::Color(palette.background.weak.color)),
            text_color: palette.background.base.text,
            border: Border::default(),
            ..Style::default()
        },
        Status::Pressed => Style {
            background: Some(Background::Color(palette.background.strong.color)),
            text_color: palette.background.base.text,
            border: Border::default(),
            ..Style::default()
        },
    }
}

/// Active (open) menu-bar tab: highlighted background.
fn menu_tab_active_style(theme: &Theme, status: Status) -> Style {
    let palette = theme.extended_palette();
    let bg = match status {
        Status::Pressed => palette.background.strong.color,
        _ => palette.background.weak.color,
    };
    Style {
        background: Some(Background::Color(bg)),
        text_color: palette.background.base.text,
        border: Border::default(),
        ..Style::default()
    }
}

/// Dropdown menu item style: full-width, flat, highlight on hover.
fn menu_item_style(theme: &Theme, status: Status) -> Style {
    let palette = theme.extended_palette();
    match status {
        Status::Active | Status::Disabled => Style {
            background: None,
            text_color: palette.background.base.text,
            border: Border::default(),
            ..Style::default()
        },
        Status::Hovered => Style {
            background: Some(Background::Color(palette.primary.base.color)),
            text_color: Color::WHITE,
            border: Border::default(),
            ..Style::default()
        },
        Status::Pressed => Style {
            background: Some(Background::Color(palette.primary.strong.color)),
            text_color: Color::WHITE,
            border: Border::default(),
            ..Style::default()
        },
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Render just the menu bar row (always visible at the top).
pub fn menu_bar<'a>(file_menu_open: bool) -> Element<'a, Message> {
    let file_button = button::Button::new(text("File").size(13))
        .on_press(Message::ToggleFileMenu)
        .padding([4, 10])
        .style(if file_menu_open {
            menu_tab_active_style as fn(&Theme, Status) -> Style
        } else {
            menu_tab_style as fn(&Theme, Status) -> Style
        });

    row![file_button].padding([2, 4]).into()
}

/// Render the dropdown panel (only when `file_menu_open` is true).
/// This is meant to be layered ON TOP of the content via a `Stack`.
///
/// Returns `None` if the menu is closed.
pub fn dropdown<'a>(file_menu_open: bool) -> Option<Element<'a, Message>> {
    if !file_menu_open {
        return None;
    }

    let item = |label: &str, msg: Message| {
        button::Button::new(text(label.to_string()).size(13))
            .on_press(msg)
            .padding([5, 20])
            .width(Length::Fill)
            .style(menu_item_style as fn(&Theme, button::Status) -> button::Style)
    };

    let panel = container(
        column![
            item("Open…", Message::OpenFile),
            item("Close", Message::CloseFile),
            item("Quit", Message::Quit),
        ]
        .width(150),
    )
    .padding(Padding::from(2))
    .style(container::bordered_box);

    // Position: flush left under the "File" tab, with a small offset.
    let positioned = container(panel).padding(Padding {
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
        left: 6.0,
    });

    Some(positioned.into())
}
