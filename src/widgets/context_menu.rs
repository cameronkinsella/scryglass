//! Context menu widget: right-click menu on the image area.
//!
//! Renders a floating panel at the cursor position with options:
//! toolbar toggle, copy image/path/filename, open location, properties.

use iced::widget::button::{self, Status, Style};
use iced::widget::{column, container, row, rule, text, toggler};
use iced::{Background, Border, Color, Element, Length, Padding, Theme};

use crate::app::Message;

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

/// Menu item style: full-width, flat, highlight on hover.
fn ctx_item_style(theme: &Theme, status: Status) -> Style {
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

/// Render the context menu at the given position.
///
/// `pos` is the cursor position relative to the overlay origin.
/// `show_toolbar` is the current toolbar visibility state.
pub fn context_menu<'a>(pos: iced::Point, show_toolbar: bool) -> Element<'a, Message> {
    use iced_fonts::bootstrap;

    let item =
        |icon_fn: fn() -> iced::widget::Text<'a>, label: &str, msg: Message| -> Element<'a, Message> {
            let content = row![
                icon_fn().size(14).width(Length::Fixed(20.0)),
                text(label.to_string()).size(13),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);

            button::Button::new(content)
                .on_press(msg)
                .padding([5, 12])
                .width(Length::Fill)
                .style(ctx_item_style as fn(&Theme, button::Status) -> button::Style)
                .into()
        };

    let toolbar_toggle: Element<'a, Message> = toggler(show_toolbar)
        .label("Toolbar")
        .on_toggle(|_| Message::ToggleToolbar)
        .size(14)
        .text_size(13)
        .into();

    let toolbar_row = container(toolbar_toggle).padding([4, 12]);

    let panel = container(
        column![
            toolbar_row,
            rule::horizontal(1),
            item(bootstrap::image, "Copy image", Message::CopyImage),
            item(bootstrap::clipboard, "Copy file path", Message::CopyFilePath),
            item(bootstrap::file_earmark, "Copy filename", Message::CopyFilename),
            rule::horizontal(1),
            item(bootstrap::folder, "Open image location", Message::OpenImageLocation),
            item(bootstrap::info_circle, "Image properties", Message::ImageProperties),
        ]
        .width(220),
    )
    .padding(Padding::from(2))
    .style(container::bordered_box);

    // Position the panel at the cursor location.
    container(panel)
        .padding(Padding {
            top: pos.y,
            right: 0.0,
            bottom: 0.0,
            left: pos.x,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}



