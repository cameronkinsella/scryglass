//! Footer zoom pop-up: a log slider for coarse zoom, plus +/- buttons that
//! step one whole percent.
//!
//! Semi-modal: an `opaque` backdrop blocks the image and filmstrip below, but
//! the footer stays live and scroll still zooms.

use iced::widget::{button, column, container, mouse_area, opaque, space, text, vertical_slider};
use iced::{Alignment, Element, Length, Padding};

use crate::app::{App, Message, ViewerMessage, ZOOM_MAX, ZOOM_MIN};
use crate::components::empty;
use crate::ui::theme;

/// Fixed width, sized for the widest readout ("5000%") so the panel doesn't
/// resize as the percentage changes.
const PANEL_WIDTH: f32 = 58.0;

/// Footer strip left uncovered, so the zoom button stays live while open.
const FOOTER_RESERVE: f32 = 26.0;

pub(crate) fn view(app: &App) -> Element<'_, Message> {
    if !app.zoom_slider_open {
        return empty();
    }
    let Some(zoom) = app
        .viewer()
        .and_then(|v| v.displayed.original_size().map(|_| v.zoom))
    else {
        return empty();
    };
    let pct = (zoom * 100.0).round() as u32;

    let step = |label: &'static str, dir: i32| {
        button(text(label).size(16))
            .on_press(Message::Viewer(ViewerMessage::NudgeZoom(dir)))
            .padding([0, 10])
            .style(theme::menu_item)
    };

    // Log scale: even fine control across the whole zoom range.
    let slider = vertical_slider(ZOOM_MIN.ln()..=ZOOM_MAX.ln(), zoom.ln(), |v| {
        Message::Viewer(ViewerMessage::SetZoom(v.exp()))
    })
    .step(0.01_f32)
    .height(Length::Fixed(150.0));

    let panel = container(
        column![
            step("+", 1),
            slider,
            step("-", -1),
            text(format!("{pct}%")).size(12),
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .spacing(6),
    )
    .width(Length::Fixed(PANEL_WIDTH))
    .padding(8)
    .style(theme::panel);

    // Opaque so a press on the panel chrome doesn't fall through to close it.
    let positioned = container(opaque(panel))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::End)
        .padding(Padding {
            top: 0.0,
            right: 99.0,
            bottom: 8.0,
            left: 0.0,
        });

    // Scroll is routed to zoom, since the opaque backdrop would otherwise
    // swallow the wheel.
    let backdrop = opaque(
        mouse_area(positioned)
            .on_press(Message::Viewer(ViewerMessage::CloseZoomSlider))
            .on_scroll(|delta| {
                let y = match delta {
                    iced::mouse::ScrollDelta::Lines { y, .. } => y,
                    iced::mouse::ScrollDelta::Pixels { y, .. } => {
                        if y > 0.0 {
                            1.0
                        } else if y < 0.0 {
                            -1.0
                        } else {
                            0.0
                        }
                    }
                };
                Message::Viewer(ViewerMessage::ScrollZoom(y))
            }),
    );

    // Closes on a footer click while staying transparent to the button's hover.
    let footer_strip = mouse_area(
        space::vertical()
            .width(Length::Fill)
            .height(Length::Fixed(FOOTER_RESERVE)),
    )
    .on_press(Message::Viewer(ViewerMessage::CloseZoomSlider));

    column![backdrop, footer_strip].into()
}

#[cfg(test)]
mod tests {
    use super::view;
    use crate::app::state::DisplayedImage;
    use crate::app::test_support::{thumb, viewing_app};
    use iced_test::simulator;

    #[test]
    fn shows_the_zoom_percentage_when_open() {
        let mut app = viewing_app(&["a.png"], 0);
        app.zoom_slider_open = true;
        {
            let v = app.viewer_mut().unwrap();
            v.displayed = DisplayedImage::Placeholder(thumb(800, 600));
            v.zoom = 0.62;
        }
        let mut ui = simulator(view(&app));
        assert!(ui.find("62%").is_ok());
    }

    #[test]
    fn renders_nothing_when_closed() {
        let app = viewing_app(&["a.png"], 0);
        let mut ui = simulator(view(&app));
        assert!(ui.find("62%").is_err());
    }
}
