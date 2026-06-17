//! Context menu widget: right-click menu on the image area.
//!
//! Renders a floating panel at the cursor position with options:
//! toolbar toggle, copy image/path/filename, open location, properties.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Element, Length, Padding, Point, Size};

use crate::app::{ContextMenuMessage, Message, ModalMessage, ToolbarMessage};
use crate::ui::theme;

/// Approximate rendered size of the context menu panel, used for edge
/// clamping. A small estimation error only shifts the menu by a few pixels.
pub const MENU_SIZE: Size = Size {
    width: 224.0,
    height: 282.0,
};

/// Clamp a desired menu position so the panel stays inside `bounds`.
///
/// If the menu is larger than the bounds, it pins to the top/left edge.
pub fn clamp_menu_pos(pos: Point, menu_size: Size, bounds: Size) -> Point {
    Point::new(
        pos.x.min(bounds.width - menu_size.width).max(0.0),
        pos.y.min(bounds.height - menu_size.height).max(0.0),
    )
}

/// Render the context menu at the given position.
///
/// `pos` is the cursor position relative to the overlay origin.
/// `show_toolbar` is the current toolbar visibility state.
pub fn context_menu<'a>(
    pos: iced::Point,
    show_toolbar: bool,
    can_modify: bool,
) -> Element<'a, Message> {
    use crate::ui::icons;

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
        .on_toggle(|_| Message::Toolbar(ToolbarMessage::ToggleToolbar))
        .size(14)
        .text_size(13)
        .into();

    let toolbar_row = container(toolbar_toggle).padding([4, 12]);

    let mut entries = column![
        toolbar_row,
        rule::horizontal(1),
        item(
            icons::image,
            "Copy image",
            Message::ContextMenu(ContextMenuMessage::CopyImage),
        ),
        item(
            icons::file_earmark,
            "Copy file",
            Message::ContextMenu(ContextMenuMessage::CopyFile),
        ),
        item(
            icons::clipboard,
            "Copy file path",
            Message::ContextMenu(ContextMenuMessage::CopyFilePath),
        ),
        item(
            icons::file_earmark,
            "Copy filename",
            Message::ContextMenu(ContextMenuMessage::CopyFilename),
        ),
        rule::horizontal(1),
        item(
            icons::folder,
            "Open image location",
            Message::ContextMenu(ContextMenuMessage::OpenImageLocation),
        ),
        item(
            icons::info_circle,
            "Image properties",
            Message::ContextMenu(ContextMenuMessage::ImageProperties),
        ),
    ]
    .width(220);

    // File modification, hidden entirely in read-only mode or archives.
    if can_modify {
        entries = entries
            .push(rule::horizontal(1))
            .push(item(
                icons::pencil_square,
                "Rename",
                Message::Modal(ModalMessage::RequestRename),
            ))
            .push(item(
                icons::trash,
                "Delete",
                Message::Modal(ModalMessage::RequestDelete),
            ));
    }

    let panel = container(entries)
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

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Size = Size {
        width: 800.0,
        height: 600.0,
    };
    const MENU: Size = Size {
        width: 200.0,
        height: 150.0,
    };

    #[test]
    fn clamp_keeps_interior_position_unchanged() {
        let pos = Point::new(100.0, 100.0);
        assert_eq!(clamp_menu_pos(pos, MENU, BOUNDS), pos);
    }

    #[test]
    fn clamp_shifts_left_at_right_edge() {
        let pos = Point::new(750.0, 100.0);
        assert_eq!(clamp_menu_pos(pos, MENU, BOUNDS), Point::new(600.0, 100.0));
    }

    #[test]
    fn clamp_shifts_up_at_bottom_edge() {
        let pos = Point::new(100.0, 580.0);
        assert_eq!(clamp_menu_pos(pos, MENU, BOUNDS), Point::new(100.0, 450.0));
    }

    #[test]
    fn clamp_handles_corner() {
        let pos = Point::new(799.0, 599.0);
        assert_eq!(clamp_menu_pos(pos, MENU, BOUNDS), Point::new(600.0, 450.0));
    }

    #[test]
    fn clamp_pins_to_origin_when_menu_exceeds_bounds() {
        let tiny = Size {
            width: 100.0,
            height: 80.0,
        };
        let pos = Point::new(50.0, 50.0);
        assert_eq!(clamp_menu_pos(pos, MENU, tiny), Point::new(0.0, 0.0));
    }
}
