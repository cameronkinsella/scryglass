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
        show_info: app.config.show_info,
    };

    let content = match &app.session {
        Session::Empty => ui::image_display::drop_prompt(),
        Session::Viewing(viewer) => {
            // Invariant: the image area only ever shows the file in the
            // title bar. When nothing is ready for that file it is empty.
            // Title, slider, and image must never diverge.
            debug_assert!(
                matches!(viewer.displayed, DisplayedImage::None)
                    || viewer.displayed_path.as_deref() == Some(viewer.nav.current()),
                "image area diverged from the navigation cursor"
            );

            // Full images and blurred placeholders render through the same
            // path: zoom/pan run on the true dimensions either way. With
            // nothing ready yet, the viewport stays honestly empty (the
            // spinner overlay appears after its grace period).
            let image_view: Element<'_, Message> = match &viewer.displayed {
                DisplayedImage::None => ui::image_display::empty_viewport(),
                DisplayedImage::Full {
                    allocation,
                    original_size,
                } => {
                    let texture = allocation.size();
                    ui::image_display::image_display(
                        allocation.handle(),
                        (texture.width, texture.height),
                        *original_size,
                        viewer.zoom,
                        viewer.pan,
                        (app.viewport_size.width, app.viewport_size.height),
                        app.config.crisp_pixels,
                    )
                }
                DisplayedImage::Placeholder(thumb) => {
                    // Placeholders always smooth: the bilinear upscale IS
                    // the blur.
                    ui::image_display::image_display(
                        &thumb.handle,
                        thumb.size,
                        thumb.original_size,
                        viewer.zoom,
                        viewer.pan,
                        (app.viewport_size.width, app.viewport_size.height),
                        false,
                    )
                }
            };

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

            // Info panel sits beside the image (not over it).
            let image_cell: Element<'_, Message> = if !app.fullscreen && app.config.show_info {
                let file_name = viewer
                    .nav
                    .current()
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let details: Vec<(String, String)> = vec![
                    (
                        "Dimensions".to_string(),
                        viewer
                            .displayed
                            .original_size()
                            .map(|(w, h)| ui::format_dimensions(w, h))
                            .unwrap_or_else(|| "…".to_string()),
                    ),
                    (
                        "File size".to_string(),
                        ui::file_size_label(viewer.current_file_size),
                    ),
                    ("Position".to_string(), viewer.nav.position_label()),
                ];
                let exif = viewer
                    .exif
                    .as_ref()
                    .filter(|(p, _)| p.as_path() == viewer.nav.current())
                    .map(|(_, fields)| fields.as_slice());
                iced::widget::row![
                    interactive,
                    ui::info_panel::info_panel(&file_name, &details, exif)
                ]
                .into()
            } else {
                interactive.into()
            };

            // The chrome below renders in every display state. It must
            // never flash away while an image loads.
            let mut col = column![image_cell];

            if !app.fullscreen && app.config.show_filmstrip {
                col = col.push(ui::filmstrip::filmstrip(
                    viewer.nav.files(),
                    viewer.nav.cursor(),
                    &viewer.thumbs,
                    viewer.filmstrip_scroll_x,
                    app.window_size.width,
                ));
            }
            if !app.fullscreen && app.config.show_slider {
                // The thumb follows the hand during a drag, the cursor otherwise.
                let value = viewer
                    .slider_drag
                    .map(|d| d.target)
                    .unwrap_or_else(|| viewer.nav.cursor());
                col = col.push(ui::nav_slider::nav_slider(value, viewer.nav.len()));
            }
            if !app.fullscreen && app.config.show_footer {
                let dims = viewer
                    .displayed
                    .original_size()
                    .map(|(w, h)| ui::format_dimensions(w, h))
                    .unwrap_or_else(|| "…".to_string());
                let zoom = if viewer.displayed.original_size().is_some() {
                    format!("{}%", (viewer.zoom * 100.0).round() as u32)
                } else {
                    "…".to_string()
                };
                let loading = viewer
                    .pending_since
                    .map(|since| since.elapsed())
                    .filter(|elapsed| *elapsed >= SPINNER_DELAY);
                let footer = ui::footer::footer(
                    &dims,
                    &ui::file_size_label(viewer.current_file_size),
                    &zoom,
                    &viewer.nav.position_label(),
                    loading,
                );
                col = col.push(footer);
            }

            col.into()
        }
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
        app.config.crisp_pixels,
        app.config.sort_key,
        app.config.sort_desc,
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
        let toolbar_offset = if app.config.show_toolbar && !app.fullscreen {
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

    // Centered spinner only when the viewport has nothing at all to show
    // (the very first load). Once an image is up, the footer's small
    // spinner takes over so nothing covers the picture.
    let spinner_overlay: Element<'_, Message> = match app.viewer() {
        Some(viewer)
            if matches!(viewer.displayed, DisplayedImage::None)
                && viewer
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

    // Scrub preview bubble: only during a slider drag that has crossed a
    // file that can't be shown live (sticky for the rest of the drag).
    let bubble_overlay: Element<'_, Message> = match app.viewer() {
        Some(viewer) => match viewer.slider_drag {
            Some(drag) if drag.bubble => ui::nav_slider::scrub_bubble(
                viewer.nav.files(),
                drag.target,
                &viewer.thumbs,
                app.window_size,
                app.config.show_footer,
            ),
            _ => column![].width(Length::Fill).height(Length::Fill).into(),
        },
        None => column![].width(Length::Fill).height(Length::Fill).into(),
    };

    let toasts = ui::toast::toast_stack(&app.toasts);

    let stacked = Stack::with_children(vec![
        content,
        spinner_overlay,
        bubble_overlay,
        toolbar_overlay,
        ctx_overlay,
        toasts,
    ]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if !app.fullscreen && app.config.show_toolbar {
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
