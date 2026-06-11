//! View function: assembles toolbar, content area, overlays, and footer.

use iced::widget::{Stack, center, column, mouse_area};
use iced::{Element, Length, mouse};

use crate::ui;
use crate::ui::toolbar::LayoutVisibility;

use super::state::{DisplayedImage, Session};
use super::{App, Message, SPINNER_DELAY, TOOLBAR_HEIGHT};

/// View function: assembles toolbar, content area, and footer.
pub fn view(app: &App) -> Element<'_, Message> {
    let layout_vis = LayoutVisibility {
        show_filmstrip: app.config.show_filmstrip,
        show_slider: app.config.show_slider,
        show_footer: app.config.show_footer,
    };

    let content = match &app.session {
        Session::Empty => ui::image_display::drop_prompt(),
        Session::Viewing(viewer) => match &viewer.displayed {
            DisplayedImage::Full { .. } | DisplayedImage::Placeholder(_) => {
                // Full images and blurred placeholders render through the
                // same path: zoom/pan run on the true dimensions either way.
                let (handle, texture_size, original_size, pixelated) = match &viewer.displayed {
                    DisplayedImage::Full {
                        allocation,
                        original_size,
                    } => {
                        let texture = allocation.size();
                        (
                            allocation.handle(),
                            (texture.width, texture.height),
                            *original_size,
                            app.config.pixelated_zoom,
                        )
                    }
                    DisplayedImage::Placeholder(thumb) => {
                        // Placeholders always smooth: the bilinear upscale
                        // IS the blur.
                        (&thumb.handle, thumb.size, thumb.original_size, false)
                    }
                    DisplayedImage::None => unreachable!(),
                };
                let zoom_pct = (viewer.zoom * 100.0).round() as u32;

                let image_view = ui::image_display::image_display(
                    handle,
                    texture_size,
                    original_size,
                    viewer.zoom,
                    viewer.pan,
                    (app.viewport_size.width, app.viewport_size.height),
                    pixelated,
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
                        &ui::format_dimensions(original_size.0, original_size.1),
                        &ui::file_size_label(viewer.current_file_size),
                        zoom_pct,
                        &viewer.nav.position_label(),
                    );
                    col = col.push(footer);
                }

                col.into()
            }
            DisplayedImage::None => ui::image_display::loading_prompt(),
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
        app.config.pixelated_zoom,
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

    // Loading spinner: appears only after a grace period so fast loads
    // never flash UI.
    let spinner_overlay: Element<'_, Message> = match app.viewer() {
        Some(viewer)
            if viewer
                .pending_since
                .is_some_and(|since| since.elapsed() >= SPINNER_DELAY) =>
        {
            let elapsed = viewer
                .pending_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
            center(ui::spinner::spinner(elapsed)).into()
        }
        _ => column![].width(Length::Fill).height(Length::Fill).into(),
    };

    let toasts = ui::toast::toast_stack(&app.toasts);

    let stacked = Stack::with_children(vec![
        content,
        spinner_overlay,
        toolbar_overlay,
        ctx_overlay,
        toasts,
    ]);

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
