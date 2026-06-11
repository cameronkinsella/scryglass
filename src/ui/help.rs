//! Keyboard shortcut help overlay, toggled with `?`.

use iced::widget::{center, column, container, row, text};
use iced::{Element, Length};

use crate::app::Message;
use crate::ui::theme;

const SHORTCUTS: &[(&str, &str)] = &[
    ("← → or A D", "Previous / next image (hold to scroll)"),
    ("Home / End", "First / last image"),
    ("Scroll or + −", "Zoom (toward cursor / center)"),
    ("Ctrl+0", "Reset zoom"),
    ("Ctrl+1", "Zoom to 100%"),
    ("Double-click", "Reset zoom"),
    ("Drag", "Pan when zoomed in"),
    ("F or F11", "Fullscreen"),
    ("I", "Info panel"),
    ("R / Shift+R", "Rotate view"),
    ("Delete", "Move to Recycle Bin"),
    ("F2", "Rename"),
    ("Right-click", "Context menu"),
    ("?", "This help"),
    ("Esc", "Close help / leave fullscreen / dismiss menus"),
];

/// Render the centered help card.
pub fn help_overlay<'a>() -> Element<'a, Message> {
    let mut rows = column![text("Keyboard shortcuts").size(16)]
        .spacing(8)
        .padding(18);

    for (keys, action) in SHORTCUTS {
        rows = rows.push(
            row![
                text(*keys).size(13).width(Length::Fixed(130.0)),
                text(*action).size(13).style(theme::secondary_text),
            ]
            .spacing(12),
        );
    }

    center(container(rows).style(theme::panel))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
