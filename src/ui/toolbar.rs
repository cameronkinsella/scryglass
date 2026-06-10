//! Toolbar widget: "File", "Zoom", and "Layout" dropdown menus.
//!
//! Renders a menu-bar style row. Clicking a tab toggles a dropdown that
//! floats over the content below. Menu items are flat, borderless buttons
//! with a subtle hover highlight, matching the look of a native menu bar.
//! All colors come from [`crate::ui::theme`].

use iced::widget::{button, column, container, row, text, toggler};
use iced::{Element, Length, Padding};

use crate::app::Message;
use crate::config::ZoomMode;
use crate::ui::theme;

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
) -> Option<Element<'a, Message>> {
    let open = open_menu?;

    let item = |label: &str, msg: Message| {
        button(text(label.to_string()).size(13))
            .on_press(msg)
            .padding([5, 20])
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
                .width(150),
            )
            .padding(Padding::from(2))
            .style(theme::panel);

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
                .style(theme::panel);

            let positioned = container(panel).padding(Padding {
                top: 0.0,
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
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: 100.0,
            });

            Some(positioned.into())
        }
    }
}
