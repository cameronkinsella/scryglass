//! Main viewer content: image/video area plus info panel, filmstrip,
//! slider, and footer chrome.

use iced::widget::{Stack, column, mouse_area};
use iced::{Element, mouse};

use crate::app::state::{DisplayedImage, Session};
use crate::app::{App, Message, SPINNER_DELAY};
use crate::ui;

pub(super) fn content(app: &App) -> Element<'_, Message> {
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

            if !app.fullscreen {
                if app.config.show_filmstrip {
                    col = col.push(ui::filmstrip::filmstrip(
                        viewer.nav.files(),
                        viewer.nav.cursor(),
                        &viewer.thumbs,
                        viewer.filmstrip_scroll_x,
                        app.window_size.width,
                    ));
                }
                if app.config.show_slider {
                    // The thumb follows the hand during a drag, the cursor otherwise.
                    let value = viewer
                        .slider_drag
                        .map(|d| d.target)
                        .unwrap_or_else(|| viewer.nav.cursor());
                    col = col.push(ui::nav_slider::nav_slider(value, viewer.nav.len()));
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

                    let footer = ui::footer::footer(
                        &dims,
                        &ui::file_size_label(viewer.current_file_size), // TODO probe to load this if held for fixed duration
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
    };

    // Optional checkerboard behind the image reveals transparency.
    let image_view: Element<'_, Message> =
        if app.config.show_checkerboard && !matches!(viewer.displayed, DisplayedImage::None) {
            Stack::with_children(vec![ui::checkerboard::checkerboard(), image_view]).into()
        } else {
            image_view
        };

    // Video transport controls: visible while paused, mid-seek, or for a
    // few seconds after any mouse movement.
    let controls_alive = viewer
        .video_controls_until
        .is_some_and(|until| iced::time::Instant::now() < until);
    match &viewer.video {
        Some(session) if !session.playing || viewer.video_seek_drag.is_some() || controls_alive => {
            let controls = ui::video_controls::video_controls(ui::video_controls::VideoControls {
                playing: session.playing,
                position: session.position(),
                duration: session.duration(),
                seek_drag: viewer.video_seek_drag,
                volume: session.volume,
                muted: session.muted,
                looping: session.looping,
            });
            Stack::with_children(vec![image_view, controls]).into()
        }
        _ => image_view,
    }
}
