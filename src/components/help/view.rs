//! Keyboard shortcut help overlay, toggled with `?`.

use iced::widget::{column, container, row, text};
use iced::{Element, Length};

use crate::app::{Message, ViewerMessage};
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
    ("T", "Toolbar"),
    ("Ctrl+C", "Copy image"),
    ("Ctrl+Shift+C", "Copy file path"),
    ("Ctrl+O", "Open file"),
    ("R / Shift+R", "Rotate view"),
    ("Delete", "Move to Recycle Bin"),
    ("F2", "Rename"),
    ("Right-click", "Context menu"),
    ("?", "This help"),
    ("Esc", "Close help / leave fullscreen / dismiss menus"),
];

#[cfg(feature = "video")]
const VIDEO_SHORTCUTS: &[(&str, &str)] = &[
    ("Space", "Play / pause"),
    ("Up / Down", "Volume"),
    ("M", "Mute"),
    ("J / L", "Back / forward 10s"),
    (". / ,", "Next / previous frame"),
];

/// Render the centered help card.
pub fn help_overlay<'a>() -> Element<'a, Message> {
    let mut rows = column![text("Keyboard shortcuts").size(16)]
        .spacing(8)
        .padding(18)
        .width(Length::Fixed(480.0));
    rows = rows.push(text("General").size(14).style(theme::accent_text));

    for &(keys, action) in SHORTCUTS {
        rows = rows.push(shortcut_row(keys, action));
    }

    #[cfg(feature = "video")]
    {
        rows = rows.push(iced::widget::rule::horizontal(1));
        rows = rows.push(text("Video").size(14).style(theme::accent_text));
        for &(keys, action) in VIDEO_SHORTCUTS {
            rows = rows.push(shortcut_row(keys, action));
        }
    }

    crate::ui::overlay_card(
        container(rows).style(theme::panel),
        Message::Viewer(ViewerMessage::ToggleHelp),
    )
}

/// One key/action line in the help card.
fn shortcut_row<'a>(keys: &'a str, action: &'a str) -> Element<'a, Message> {
    row![
        text(keys).size(13).width(Length::Fixed(130.0)),
        text(action).size(13).style(theme::secondary_text),
    ]
    .spacing(12)
    .into()
}

#[cfg(test)]
mod tests {
    use super::help_overlay;
    use iced_test::simulator;

    #[test]
    fn renders_the_shortcut_list() {
        let mut ui = simulator(help_overlay());
        assert!(ui.find("Keyboard shortcuts").is_ok());
        assert!(ui.find("General").is_ok());
        assert!(ui.find("First / last image").is_ok());
    }

    #[cfg(feature = "video")]
    #[test]
    fn shows_the_video_section() {
        let mut ui = simulator(help_overlay());
        assert!(ui.find("Video").is_ok());
        assert!(ui.find("Play / pause").is_ok());
    }
}
