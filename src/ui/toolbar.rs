//! Toolbar widget: "File" and "Zoom" dropdown menus.
//!
//! Renders a menu-bar style row. Clicking "File" or "Zoom" toggles a dropdown
//! that floats over the content below. Menu items are flat, borderless buttons
//! with a subtle hover highlight, matching the look of a native menu bar.

use iced::widget::button::{self, Status, Style};
use iced::widget::{column, container, row, text, toggler};
use iced::{Background, Border, Color, Element, Length, Padding, Theme};

use crate::app::Message;
use crate::config::ZoomMode;

/// Layout visibility state passed into dropdown rendering.
#[derive(Debug, Clone, Copy)]
pub struct LayoutVisibility {
    pub show_filmstrip: bool,
    pub show_slider: bool,
    pub show_footer: bool,
}

/// Which toolbar dropdown is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMenu {
    File,
    Zoom,
    Layout,
}

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
pub fn menu_bar<'a>(open_menu: Option<OpenMenu>) -> Element<'a, Message> {
    let file_button = button::Button::new(text("File").size(13))
        .on_press(Message::ToggleFileMenu)
        .padding([4, 10])
        .style(if open_menu == Some(OpenMenu::File) {
            menu_tab_active_style as fn(&Theme, Status) -> Style
        } else {
            menu_tab_style as fn(&Theme, Status) -> Style
        });

    let zoom_button = button::Button::new(text("Zoom").size(13))
        .on_press(Message::ToggleZoomMenu)
        .padding([4, 10])
        .style(if open_menu == Some(OpenMenu::Zoom) {
            menu_tab_active_style as fn(&Theme, Status) -> Style
        } else {
            menu_tab_style as fn(&Theme, Status) -> Style
        });

    let layout_button = button::Button::new(text("Layout").size(13))
        .on_press(Message::ToggleLayoutMenu)
        .padding([4, 10])
        .style(if open_menu == Some(OpenMenu::Layout) {
            menu_tab_active_style as fn(&Theme, Status) -> Style
        } else {
            menu_tab_style as fn(&Theme, Status) -> Style
        });

    row![file_button, zoom_button, layout_button]
        .padding([2, 4])
        .into()
}

/// Render the dropdown panel if a menu is open.
/// This is meant to be layered ON TOP of the content via a `Stack`.
///
/// Returns `None` if no menu is open.
pub fn dropdown<'a>(
    open_menu: Option<OpenMenu>,
    current_zoom_mode: ZoomMode,
    layout_vis: LayoutVisibility,
) -> Option<Element<'a, Message>> {
    let open = open_menu?;

    let item = |label: &str, msg: Message| {
        button::Button::new(text(label.to_string()).size(13))
            .on_press(msg)
            .padding([5, 20])
            .width(Length::Fill)
            .style(menu_item_style as fn(&Theme, button::Status) -> button::Style)
    };

    match open {
        OpenMenu::File => {
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

            let positioned = container(panel).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 6.0,
            });

            Some(positioned.into())
        }
        OpenMenu::Zoom => {
            let mut items: Vec<Element<'a, Message>> = Vec::new();
            for &mode in ZoomMode::ALL {
                let prefix = if mode == current_zoom_mode {
                    "● "
                } else {
                    "   "
                };
                let label = format!("{prefix}{}", mode.label());
                items.push(item(&label, Message::SetZoomMode(mode)).into());
            }

            let panel = container(column(items).width(180))
                .padding(Padding::from(2))
                .style(container::bordered_box);

            let positioned = container(panel).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 52.0,
            });

            Some(positioned.into())
        }
        OpenMenu::Layout => {
            let panel = container(
                column![
                    toggler(layout_vis.show_filmstrip)
                        .label("Filmstrip")
                        .on_toggle(|_| Message::ToggleFilmstrip)
                        .size(16)
                        .text_size(13),
                    toggler(layout_vis.show_slider)
                        .label("Slider")
                        .on_toggle(|_| Message::ToggleSlider)
                        .size(16)
                        .text_size(13),
                    toggler(layout_vis.show_footer)
                        .label("Footer")
                        .on_toggle(|_| Message::ToggleFooter)
                        .size(16)
                        .text_size(13),
                ]
                .spacing(6)
                .padding([6, 12])
                .width(180),
            )
            .style(container::bordered_box);

            // Position under the "Layout" tab.
            let positioned = container(panel).padding(Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 100.0,
            });

            Some(positioned.into())
        }
    }
}
