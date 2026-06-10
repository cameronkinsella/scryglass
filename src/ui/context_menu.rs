//! Context menu widget: right-click menu on the image area.
//!
//! Renders a floating panel at the cursor position with options:
//! toolbar toggle, copy image/path/filename, open location, properties.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Element, Length, Padding};

use crate::app::Message;
use crate::ui::theme;

/// Render the context menu at the given position.
///
/// `pos` is the cursor position relative to the overlay origin.
/// `show_toolbar` is the current toolbar visibility state.
pub fn context_menu<'a>(pos: iced::Point, show_toolbar: bool) -> Element<'a, Message> {
    use iced_fonts::bootstrap;

    let item = |icon_fn: fn() -> iced::widget::Text<'a>,
                label: &str,
                msg: Message|
     -> Element<'a, Message> {
        let content = row![
            icon_fn().size(14).width(Length::Fixed(20.0)),
            text(label.to_string()).size(13),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);

        button(content)
            .on_press(msg)
            .padding([5, 12])
            .width(Length::Fill)
            .style(theme::menu_item)
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
            item(
                bootstrap::clipboard,
                "Copy file path",
                Message::CopyFilePath
            ),
            item(
                bootstrap::file_earmark,
                "Copy filename",
                Message::CopyFilename
            ),
            rule::horizontal(1),
            item(
                bootstrap::folder,
                "Open image location",
                Message::OpenImageLocation
            ),
            item(
                bootstrap::info_circle,
                "Image properties",
                Message::ImageProperties
            ),
        ]
        .width(220),
    )
    .padding(Padding::from(2))
    .style(theme::panel);

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
