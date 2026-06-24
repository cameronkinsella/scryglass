use iced::widget::{Space, Stack, center, column, container, mouse_area};
use iced::{Element, Length, mouse};

use crate::app::state::{Direction, DisplayedImage, Session, Viewer};
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

            let media: Element<'_, Message> = if edge_nav_active(app, viewer, hide_cursor) {
                // Keep the strips clear of the video transport bar at the bottom.
                let reserve = if viewer.controls_opacity > 0.0 {
                    VIDEO_CONTROLS_RESERVE
                } else {
                    0.0
                };
                Stack::with_children(vec![
                    interactive.into(),
                    edge_overlay(viewer.edge_hover, reserve),
                ])
                .into()
            } else {
                interactive.into()
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
                iced::widget::row![media, info_panel::view(&file_name, &details, exif)].into()
            } else {
                media
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

/// Bottom strip inset clearing the video transport bar (about its height).
const VIDEO_CONTROLS_RESERVE: f32 = 44.0;

// Yield to any open overlay: a strip would steal its hover and dismiss it.
fn edge_nav_active(app: &App, viewer: &Viewer, hide_cursor: bool) -> bool {
    app.config.mouse_nav
        && viewer.nav.len() > 1
        && !hide_cursor
        && app.open_menu.is_none()
        && app.context_menu_pos.is_none()
        && app.modal.is_none()
        && !app.help_open
        && !app.zoom_slider_open
}

/// Left and right edge-navigation strips over the image. `bottom_reserve`
/// keeps them clear of the video transport bar.
fn edge_overlay<'a>(hovered: Option<Direction>, bottom_reserve: f32) -> Element<'a, Message> {
    const STRIP_WIDTH: f32 = 64.0;

    let zone = |dir: Direction, side: iced::Alignment| {
        let arrow: Element<'a, Message> = if hovered == Some(dir) {
            let glyph = match dir {
                Direction::Backward => ui::icons::chevron_left(),
                Direction::Forward => ui::icons::chevron_right(),
            };
            container(glyph.size(22))
                .width(Length::Fixed(36.0))
                .height(Length::Fixed(36.0))
                .align_x(iced::Alignment::Center)
                .align_y(iced::Alignment::Center)
                .style(crate::ui::theme::edge_nav_arrow)
                .into()
        } else {
            Space::new().into()
        };
        mouse_area(
            container(arrow)
                .width(Length::Fixed(STRIP_WIDTH))
                .height(Length::Fill)
                .align_x(side)
                .align_y(iced::Alignment::Center)
                .padding(12),
        )
        .interaction(mouse::Interaction::Pointer)
        .on_enter(Message::Viewer(viewer::Message::EdgeEnter(dir)))
        .on_exit(Message::Viewer(viewer::Message::EdgeExit))
        .on_press(Message::Viewer(viewer::Message::EdgePress(dir)))
        // The strip sits above the image, so it raises the context menu itself.
        .on_right_press(Message::ContextMenu(context_menu::Message::Show))
    };

    let strips = iced::widget::row![
        zone(Direction::Backward, iced::Alignment::Start),
        Space::new().width(Length::Fill),
        zone(Direction::Forward, iced::Alignment::End),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    container(strips)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding::ZERO.bottom(bottom_reserve))
        .into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Modal;
    use crate::app::test_support::viewing_app;
    use crate::components::toolbar::OpenMenu;

    #[test]
    fn edge_nav_is_on_by_default_with_several_files() {
        let app = viewing_app(&["a.png", "b.png"], 0);
        assert!(edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn edge_nav_is_off_for_a_single_file() {
        let app = viewing_app(&["only.png"], 0);
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn edge_nav_is_off_when_the_setting_is_disabled() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.config.mouse_nav = false;
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn an_open_dropdown_stands_the_strips_down() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.open_menu = Some(OpenMenu::File);
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn an_open_context_menu_stands_the_strips_down() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn an_open_modal_stands_the_strips_down() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.modal = Some(Modal::Settings);
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), false));
    }

    #[test]
    fn an_idle_hidden_cursor_stands_the_strips_down() {
        let app = viewing_app(&["a.png", "b.png"], 0);
        assert!(!edge_nav_active(&app, app.viewer().unwrap(), true));
    }
}
