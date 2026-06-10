//! View function: assembles toolbar, content area, overlays, and footer.

use iced::widget::{Stack, column, mouse_area};
use iced::{Element, Length, mouse};

use crate::ui;
use crate::ui::toolbar::LayoutVisibility;

use super::state::Session;
use super::{App, Message, TOOLBAR_HEIGHT};

/// View function: assembles toolbar, content area, and footer.
pub fn view(app: &App) -> Element<'_, Message> {
    let layout_vis = LayoutVisibility {
        show_filmstrip: app.config.show_filmstrip,
        show_slider: app.config.show_slider,
        show_footer: app.config.show_footer,
    };

    let content = match &app.session {
        Session::Empty => ui::image_display::drop_prompt(),
        Session::Viewing(viewer) => match &viewer.current_allocation {
            Some(allocation) => {
                let size = allocation.size();
                let zoom_pct = (viewer.zoom * 100.0).round() as u32;

                let image_view = ui::image_display::image_display(
                    allocation,
                    viewer.zoom,
                    viewer.pan,
                    (app.viewport_size.width, app.viewport_size.height),
                );

                // Wrap image area in mouse_area for scroll, drag, double-click, and right-click.
                let interactive = mouse_area(image_view)
                    .on_press(Message::DragStart)
                    .on_right_press(Message::ShowContextMenu)
                    .on_scroll(|delta| {
                        let y = match delta {
                            mouse::ScrollDelta::Lines { y, .. } => y,
                            mouse::ScrollDelta::Pixels { y, .. } => {
                                if y > 0.0 {
                                    1.0
                                } else if y < 0.0 {
                                    -1.0
                                } else {
                                    0.0
                                }
                            }
                        };
                        Message::ScrollZoom(y)
                    })
                    .on_double_click(Message::ResetZoom);

                // Build the bottom section: filmstrip, slider, footer (each optional).
                let mut col = column![interactive];

                if app.config.show_filmstrip {
                    col = col.push(ui::filmstrip::filmstrip(
                        viewer.nav.files(),
                        viewer.nav.cursor(),
                    ));
                }
                if app.config.show_slider {
                    col = col.push(ui::nav_slider::nav_slider(
                        viewer.nav.cursor(),
                        viewer.nav.len(),
                    ));
                }
                if app.config.show_footer {
                    let footer = ui::footer::footer(
                        &ui::format_dimensions(size.width, size.height),
                        &ui::file_size_label(viewer.current_file_size),
                        zoom_pct,
                        &viewer.nav.position_label(),
                    );
                    col = col.push(footer);
                }

                col.into()
            }
            None => ui::image_display::loading_prompt(),
        },
    };

    // Main layout: toolbar on top (if visible), then content fills remaining space.
    // Always use Stack so the widget tree structure is stable. This
    // prevents iced from losing internal widget state (e.g. filmstrip
    // scroll position) when toggling menus.

    // Build the toolbar dropdown overlay (or invisible placeholder).
    let toolbar_overlay: Element<'_, Message> = if let Some(dropdown) = ui::toolbar::dropdown(
        app.open_menu,
        app.config.zoom_mode,
        layout_vis,
        app.config.theme == crate::config::ThemeChoice::Light,
    ) {
        column![dropdown]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        column![].width(Length::Fill).height(Length::Fill).into()
    };

    // Build the context menu overlay (or invisible placeholder).
    // The context menu is positioned inside the stacked area (below toolbar),
    // but pos is in window coordinates, so subtract toolbar height.
    let ctx_overlay: Element<'_, Message> = if let Some(pos) = app.context_menu_pos {
        let toolbar_offset = if app.config.show_toolbar {
            TOOLBAR_HEIGHT
        } else {
            0.0
        };
        let adjusted_pos = iced::Point::new(pos.x, pos.y - toolbar_offset);
        // Keep the menu inside the stacked area (window minus toolbar).
        let bounds = iced::Size::new(
            app.window_size.width,
            app.window_size.height - toolbar_offset,
        );
        let clamped =
            ui::context_menu::clamp_menu_pos(adjusted_pos, ui::context_menu::MENU_SIZE, bounds);
        ui::context_menu::context_menu(clamped, app.config.show_toolbar)
    } else {
        column![].width(Length::Fill).height(Length::Fill).into()
    };

    let stacked = Stack::with_children(vec![content, toolbar_overlay, ctx_overlay]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if app.config.show_toolbar {
        page = page.push(ui::toolbar::menu_bar(app.open_menu));
    }
    page = page.push(stacked);

    if app.context_menu_pos.is_some() {
        mouse_area(page)
            .on_press(Message::DismissContextMenu)
            .on_right_press(Message::DismissContextMenu)
            .into()
    } else if app.open_menu.is_some() {
        mouse_area(page)
            .on_press(Message::DismissOverlay)
            .on_right_press(Message::DismissOverlay)
            .into()
    } else {
        mouse_area(page).into()
    }
}
