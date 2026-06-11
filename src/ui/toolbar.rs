//! Toolbar widget: "File", "Zoom", and "Layout" dropdown menus.
//!
//! Renders a menu-bar style row. Clicking a tab toggles a dropdown that
//! floats over the content below. Menu items are flat, borderless buttons
//! with a subtle hover highlight, matching the look of a native menu bar.
//! All colors come from [`crate::ui::theme`].

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Alignment, Element, Length, Padding};

use crate::app::Message;
use crate::config::ZoomMode;
use crate::ui::{icons, theme};

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

/// Render just the menu bar row (always visible at the top).
pub fn menu_bar<'a>(open_menu: Option<OpenMenu>) -> Element<'a, Message> {
    let tab = |label: &'a str, menu: OpenMenu, msg: Message| {
        button(text(label).size(13))
            .on_press(msg)
            .padding([4, 10])
            .style(if open_menu == Some(menu) {
                theme::menu_tab_active
            } else {
                theme::menu_tab
            })
    };

    let bar = row![
        tab("File", OpenMenu::File, Message::ToggleFileMenu),
        tab("Zoom", OpenMenu::Zoom, Message::ToggleZoomMenu),
        tab("Layout", OpenMenu::Layout, Message::ToggleLayoutMenu),
    ]
    .padding([2, 4]);

    container(bar)
        .width(Length::Fill)
        .style(theme::surface)
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
    light_theme: bool,
    crisp_pixels: bool,
) -> Option<Element<'a, Message>> {
    let open = open_menu?;

    // A plain action item: label only, aligned with checkable items.
    let item = |label: &str, msg: Message| {
        button(text(label.to_string()).size(13))
            .on_press(msg)
            .padding([6, 12])
            .width(Length::Fill)
            .style(theme::menu_item)
    };

    // A checkable item: a fixed-width checkmark slot keeps labels aligned
    // whether or not the entry is selected.
    let checkable = |label: &str, selected: bool, msg: Message| {
        let check = icons::check_lg()
            .size(12)
            .width(Length::Fixed(18.0))
            .style(theme::check_indicator(selected));
        let content = row![check, text(label.to_string()).size(13)]
            .spacing(4)
            .align_y(Alignment::Center);
        button(content)
            .on_press(msg)
            .padding([6, 12])
            .width(Length::Fill)
            .style(theme::menu_item)
    };

    match open {
        OpenMenu::File => {
            let panel = container(
                column![
                    item("Open…", Message::OpenFile),
                    item("Close", Message::CloseFile),
                    item("Quit", Message::Quit),
                ]
                .width(160),
            )
            .padding(Padding::from(4))
            .style(theme::panel);

            let positioned = container(panel).padding(Padding {
                top: 2.0,
                right: 0.0,
                bottom: 0.0,
                left: 6.0,
            });

            Some(positioned.into())
        }
        OpenMenu::Zoom => {
            let mut items: Vec<Element<'a, Message>> = Vec::new();
            for &mode in ZoomMode::ALL {
                items.push(
                    checkable(
                        mode.label(),
                        mode == current_zoom_mode,
                        Message::SetZoomMode(mode),
                    )
                    .into(),
                );
            }
            items.push(rule::horizontal(1).into());
            items.push(
                checkable(
                    "Crisp pixels when zoomed",
                    crisp_pixels,
                    Message::ToggleCrispPixels,
                )
                .into(),
            );

            let panel = container(column(items).spacing(1).width(200))
                .padding(Padding::from(4))
                .style(theme::panel);

            let positioned = container(panel).padding(Padding {
                top: 2.0,
                right: 0.0,
                bottom: 0.0,
                left: 52.0,
            });

            Some(positioned.into())
        }
        OpenMenu::Layout => {
            let toggle = |label: &'a str, active: bool, msg: fn(bool) -> Message| {
                toggler(active)
                    .label(label)
                    .on_toggle(msg)
                    .size(16)
                    .text_size(13)
            };

            let panel = container(
                column![
                    toggle("Filmstrip", layout_vis.show_filmstrip, |_| {
                        Message::ToggleFilmstrip
                    }),
                    toggle("Slider", layout_vis.show_slider, |_| Message::ToggleSlider),
                    toggle("Footer", layout_vis.show_footer, |_| Message::ToggleFooter),
                    toggle("Light theme", light_theme, |_| Message::ToggleTheme),
                ]
                .spacing(6)
                .padding([6, 12])
                .width(180),
            )
            .style(theme::panel);

            // Position under the "Layout" tab.
            let positioned = container(panel).padding(Padding {
                top: 2.0,
                right: 0.0,
                bottom: 0.0,
                left: 100.0,
            });

            Some(positioned.into())
        }
    }
}
