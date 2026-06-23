//! Context menu widget: right-click menu on the image area.
//!
//! Renders a floating panel at the cursor position with options:
//! toolbar toggle, copy image/path/filename, open location, properties.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Element, Length, Padding, Point, Size};

use crate::app::{ContextMenuMessage, Message, ModalMessage, ToolbarMessage};
use crate::ui::theme;

const MENU_WIDTH: f32 = 224.0;
/// Height of one item or the toggler row (content plus padding); they all
/// render the same, so the panel height is a function of the row count.
const ROW_HEIGHT: f32 = 28.0;
const RULE_HEIGHT: f32 = 1.0;
const PANEL_PADDING: f32 = 2.0;

/// The panel's size for the current item set. The height must match what
/// renders so flip placement anchors the cursor to a corner without a gap, and
/// it grows when the editing actions (rename, delete) are shown.
pub fn menu_size(can_modify: bool) -> Size {
    // toggler + 4 copy + 2 location rows and 2 separator rules; editing adds a
    // third rule plus the rename and delete rows.
    let rows = if can_modify { 9.0 } else { 7.0 };
    let rules = if can_modify { 3.0 } else { 2.0 };
    Size::new(
        MENU_WIDTH,
        2.0 * PANEL_PADDING + rows * ROW_HEIGHT + rules * RULE_HEIGHT,
    )
}

/// Place the menu so the cursor stays on one of its corners, like a native
/// menu: it opens down and to the right of `pos`, but flips to the other side
/// of the cursor on whichever axis would overflow `bounds`. If the menu is
/// larger than `bounds` on an axis, it pins to that edge instead.
pub fn flip_menu_pos(pos: Point, menu_size: Size, bounds: Size) -> Point {
    let x = if pos.x + menu_size.width <= bounds.width {
        pos.x
    } else {
        pos.x - menu_size.width
    };
    let y = if pos.y + menu_size.height <= bounds.height {
        pos.y
    } else {
        pos.y - menu_size.height
    };
    Point::new(
        x.min(bounds.width - menu_size.width).max(0.0),
        y.min(bounds.height - menu_size.height).max(0.0),
    )
}

/// Render the context menu. `pos` is relative to the overlay origin.
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
    fn keeps_interior_position_unchanged() {
        let pos = Point::new(100.0, 100.0);
        assert_eq!(flip_menu_pos(pos, MENU, BOUNDS), pos);
    }

    #[test]
    fn opens_left_at_the_right_edge() {
        // The menu's right edge anchors to the cursor: 550 + 200 = 750.
        let pos = Point::new(750.0, 100.0);
        assert_eq!(flip_menu_pos(pos, MENU, BOUNDS), Point::new(550.0, 100.0));
    }

    #[test]
    fn opens_up_at_the_bottom_edge() {
        // The menu's bottom edge anchors to the cursor: 430 + 150 = 580.
        let pos = Point::new(100.0, 580.0);
        assert_eq!(flip_menu_pos(pos, MENU, BOUNDS), Point::new(100.0, 430.0));
    }

    #[test]
    fn flips_both_axes_in_the_corner() {
        let pos = Point::new(799.0, 599.0);
        let placed = flip_menu_pos(pos, MENU, BOUNDS);
        assert_eq!(placed, Point::new(599.0, 449.0));
        // The cursor lands exactly on the menu's bottom-right corner.
        assert_eq!(
            Point::new(placed.x + MENU.width, placed.y + MENU.height),
            pos
        );
    }

    #[test]
    fn pins_to_origin_when_menu_exceeds_bounds() {
        let tiny = Size {
            width: 100.0,
            height: 80.0,
        };
        let pos = Point::new(50.0, 50.0);
        assert_eq!(flip_menu_pos(pos, MENU, tiny), Point::new(0.0, 0.0));
    }

    #[test]
    fn menu_size_grows_with_editing_actions() {
        // The rename/delete rows make the editable menu taller.
        assert!(menu_size(true).height > menu_size(false).height);
    }

    #[test]
    fn lists_the_copy_and_location_actions() {
        use iced_test::simulator;
        let mut ui = simulator(context_menu(Point::new(10.0, 10.0), true, false));
        assert!(ui.find("Copy image").is_ok());
        assert!(ui.find("Copy file path").is_ok());
        assert!(ui.find("Open image location").is_ok());
        assert!(ui.find("Image properties").is_ok());
    }

    #[test]
    fn shows_modify_actions_only_when_allowed() {
        use iced_test::simulator;
        let mut with = simulator(context_menu(Point::new(0.0, 0.0), true, true));
        assert!(with.find("Rename").is_ok());
        assert!(with.find("Delete").is_ok());
        let mut without = simulator(context_menu(Point::new(0.0, 0.0), true, false));
        assert!(without.find("Rename").is_err());
    }
}
