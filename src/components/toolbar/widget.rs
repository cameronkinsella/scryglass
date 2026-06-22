//! Toolbar: "File", "Zoom", and "Layout" dropdown menus, styled to look
//! like a native menu bar.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Alignment, Element, Length, Padding};

use crate::app::{Message, OpenMessage, SettingsMessage, ToolbarMessage, ViewerMessage};
use crate::config::{SortKey, ZoomMode};
use crate::ui::{icons, theme};

/// Layout visibility state passed into dropdown rendering.
#[derive(Debug, Clone, Copy)]
pub struct LayoutVisibility {
    pub show_filmstrip: bool,
    pub show_slider: bool,
    pub show_footer: bool,
    pub show_info: bool,
    pub show_checkerboard: bool,
}

/// Which toolbar dropdown is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMenu {
    File,
    Zoom,
    Sort,
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
        tab(
            "File",
            OpenMenu::File,
            Message::Toolbar(ToolbarMessage::ToggleFileMenu),
        ),
        tab(
            "Zoom",
            OpenMenu::Zoom,
            Message::Toolbar(ToolbarMessage::ToggleZoomMenu),
        ),
        tab(
            "Sort",
            OpenMenu::Sort,
            Message::Toolbar(ToolbarMessage::ToggleSortMenu),
        ),
        tab(
            "Layout",
            OpenMenu::Layout,
            Message::Toolbar(ToolbarMessage::ToggleLayoutMenu),
        ),
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
    sort_key: SortKey,
    sort_desc: bool,
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
                    item("Open…", Message::Open(OpenMessage::OpenFile)),
                    item("Close", Message::Open(OpenMessage::CloseFile)),
                    rule::horizontal(1),
                    item("Settings…", Message::Settings(SettingsMessage::Open)),
                    rule::horizontal(1),
                    item("Quit", Message::Open(OpenMessage::Quit)),
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
                        Message::Toolbar(ToolbarMessage::SetZoomMode(mode)),
                    )
                    .into(),
                );
            }
            items.push(rule::horizontal(1).into());
            items.push(
                checkable(
                    "Crisp pixels when zoomed",
                    crisp_pixels,
                    Message::Toolbar(ToolbarMessage::ToggleCrispPixels),
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
        OpenMenu::Sort => {
            let mut items: Vec<Element<'a, Message>> = Vec::new();
            for &key in SortKey::ALL {
                items.push(
                    checkable(
                        key.label(),
                        key == sort_key,
                        Message::Toolbar(ToolbarMessage::SetSortKey(key)),
                    )
                    .into(),
                );
            }
            items.push(rule::horizontal(1).into());
            items.push(
                checkable(
                    "Descending",
                    sort_desc,
                    Message::Toolbar(ToolbarMessage::ToggleSortDirection),
                )
                .into(),
            );

            let panel = container(column(items).spacing(1).width(180))
                .padding(Padding::from(4))
                .style(theme::panel);

            let positioned = container(panel).padding(Padding {
                top: 2.0,
                right: 0.0,
                bottom: 0.0,
                left: 100.0,
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
                        Message::Toolbar(ToolbarMessage::ToggleFilmstrip)
                    }),
                    toggle("Slider", layout_vis.show_slider, |_| {
                        Message::Toolbar(ToolbarMessage::ToggleSlider)
                    }),
                    toggle("Footer", layout_vis.show_footer, |_| {
                        Message::Toolbar(ToolbarMessage::ToggleFooter)
                    }),
                    toggle("Info panel", layout_vis.show_info, |_| {
                        Message::Viewer(ViewerMessage::ToggleInfo)
                    }),
                    toggle("Checkerboard", layout_vis.show_checkerboard, |_| {
                        Message::Viewer(ViewerMessage::ToggleCheckerboard)
                    }),
                    toggle("Light theme", light_theme, |_| {
                        Message::Toolbar(ToolbarMessage::ToggleTheme)
                    }),
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
                left: 144.0,
            });

            Some(positioned.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SortKey, ZoomMode};
    use iced_test::simulator;

    fn vis() -> LayoutVisibility {
        LayoutVisibility {
            show_filmstrip: true,
            show_slider: true,
            show_footer: true,
            show_info: false,
            show_checkerboard: false,
        }
    }

    fn open(menu: OpenMenu) -> Element<'static, Message> {
        dropdown(
            Some(menu),
            ZoomMode::Auto,
            vis(),
            false,
            false,
            SortKey::Name,
            true,
        )
        .unwrap()
    }

    #[test]
    fn menu_bar_shows_every_tab() {
        let mut ui = simulator(menu_bar(None));
        for tab in ["File", "Zoom", "Sort", "Layout"] {
            assert!(ui.find(tab).is_ok(), "missing tab: {tab}");
        }
    }

    #[test]
    fn no_dropdown_without_an_open_menu() {
        assert!(
            dropdown(
                None,
                ZoomMode::Auto,
                vis(),
                false,
                false,
                SortKey::Name,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn file_dropdown_lists_its_actions() {
        let mut ui = simulator(open(OpenMenu::File));
        assert!(ui.find("Open…").is_ok());
        assert!(ui.find("Settings…").is_ok());
        assert!(ui.find("Quit").is_ok());
    }

    #[test]
    fn zoom_dropdown_offers_crisp_pixels() {
        let mut ui = simulator(open(OpenMenu::Zoom));
        assert!(ui.find("Crisp pixels when zoomed").is_ok());
    }

    #[test]
    fn sort_dropdown_offers_descending() {
        let mut ui = simulator(open(OpenMenu::Sort));
        assert!(ui.find("Descending").is_ok());
    }

    #[test]
    fn layout_dropdown_builds() {
        assert!(
            dropdown(
                Some(OpenMenu::Layout),
                ZoomMode::Auto,
                vis(),
                true,
                false,
                SortKey::Name,
                false
            )
            .is_some()
        );
    }
}
