use iced::widget::{Stack, center, column, mouse_area};
use iced::{Element, Length, mouse};

use crate::app::state::{DisplayedImage, Session};
use crate::app::{App, Message, SPINNER_DELAY};
use crate::components::{
    context_menu, empty, filmstrip, footer, info_panel, nav_slider, video_controls,
};
use crate::ui;

use super as viewer;

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    match &app.session {
        // Keep the drop prompt out of the way while an open is scanning.
        Session::Empty if app.opening_since.is_some() => ui::image_display::empty_viewport(),
        Session::Empty => ui::image_display::drop_prompt(),
        Session::Viewing(viewer) => {
            // Invariant: the image area shows only the title-bar file, or nothing.
            debug_assert!(
                matches!(viewer.displayed, DisplayedImage::None)
                    || viewer.displayed_path.as_deref() == Some(viewer.nav.current()),
                "image area diverged from the navigation cursor"
            );

            let image_view = image_view(app);

            let hide_cursor = crate::app::viewer_math::hide_idle_cursor(
                viewer.video.as_ref().is_some_and(|s| s.playing),
                viewer.video_seek_drag.is_some(),
                viewer.controls_opacity > 0.0,
            );

            let interactive = mouse_area(image_view)
                .on_press(Message::Viewer(viewer::Message::DragStart))
                .on_right_press(Message::ContextMenu(context_menu::Message::Show))
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
                    Message::Viewer(viewer::Message::ScrollZoom(y))
                })
                .on_double_click(Message::Viewer(viewer::Message::ResetZoom));
            let interactive = if hide_cursor {
                interactive.interaction(mouse::Interaction::Hidden)
            } else {
                interactive
            };

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
                iced::widget::row![interactive, info_panel::view(&file_name, &details, exif)].into()
            } else {
                interactive.into()
            };

            // The chrome below renders in every display state. It must
            // never flash away while an image loads.
            let mut col = column![image_cell];

            if !app.fullscreen {
                if app.config.show_filmstrip {
                    col = col.push(filmstrip::view(app));
                }
                if app.config.show_slider {
                    col = col.push(nav_slider::view(app));
                }
                if app.config.show_footer {
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

                    let footer = footer::view(
                        &dims,
                        &ui::file_size_label(viewer.current_file_size),
                        &zoom,
                        &viewer.nav.position_label(),
                        loading,
                    );
                    col = col.push(footer);
                }
            }

            col.into()
        }
    }
}

fn image_view(app: &App) -> Element<'_, Message> {
    let Some(viewer) = app.viewer() else {
        return ui::image_display::empty_viewport();
    };

    // Full images and blurred placeholders render through the same path:
    // zoom/pan run on the true dimensions either way.
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
            // Placeholders always smooth: the bilinear upscale IS the blur.
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
        DisplayedImage::Video { .. } => match viewer.video_frame.clone() {
            #[cfg(feature = "video")]
            Some(frame) => ui::video_surface::view(
                frame,
                viewer.zoom,
                viewer.pan,
                (app.viewport_size.width, app.viewport_size.height),
                app.config.crisp_pixels,
            ),
            _ => ui::image_display::empty_viewport(),
        },
        DisplayedImage::Error { message } => ui::image_display::error_viewport(message),
    };

    // Optional checkerboard behind the image reveals transparency.
    let image_view: Element<'_, Message> = if app.config.show_checkerboard
        && !matches!(
            viewer.displayed,
            DisplayedImage::None | DisplayedImage::Error { .. }
        ) {
        Stack::with_children(vec![ui::checkerboard::checkerboard(), image_view]).into()
    } else {
        image_view
    };

    // Video transport controls, faded in/out by the per-tick opacity ease.
    match &viewer.video {
        Some(session) if viewer.controls_opacity > 0.0 => Stack::with_children(vec![
            image_view,
            video_controls::view(session, viewer, viewer.controls_opacity),
        ])
        .into(),
        _ => image_view,
    }
}

pub(crate) fn spinner(app: &App) -> Element<'_, Message> {
    let footer_visible = app.config.show_footer && !app.fullscreen;
    let opening = app
        .opening_since
        .filter(|since| since.elapsed() >= SPINNER_DELAY);
    match app.viewer() {
        _ if opening.is_some() => {
            let elapsed = opening.map(|since| since.elapsed()).unwrap_or_default();
            center(ui::spinner::spinner(elapsed)).into()
        }
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
        Some(viewer)
            if !footer_visible
                && viewer
                    .pending_since
                    .is_some_and(|since| since.elapsed() >= SPINNER_DELAY) =>
        {
            let elapsed = viewer
                .pending_since
                .map(|since| since.elapsed())
                .unwrap_or_default();
            iced::widget::container(ui::spinner::spinner_sized(elapsed, 14.0))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(iced::Alignment::End)
                .align_y(iced::Alignment::End)
                .padding(12)
                .into()
        }
        _ => empty(),
    }
}
