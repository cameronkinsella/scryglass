//! Loading spinner: a small rotating arc drawn on a canvas.
//!
//! The arc's rotation is derived from how long the load has been pending,
//! so the widget itself is stateless, a lightweight subscription tick
//! triggers redraws while a load is in flight.

use std::time::Duration;

use iced::mouse;
use iced::widget::canvas::{self, Canvas, Path, Stroke};
use iced::{Element, Length, Radians, Rectangle, Renderer, Theme};

use crate::ui::theme;

/// Diameter of the spinner in logical pixels.
const SIZE: f32 = 40.0;

/// Revolutions per second.
const SPEED: f32 = 1.1;

struct Arc {
    elapsed: Duration,
}

impl<Message> canvas::Program<Message> for Arc {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        app_theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let center = frame.center();
        let radius = bounds.width.min(bounds.height) / 2.0 - 4.0;

        let turns = self.elapsed.as_secs_f32() * SPEED;
        let start = Radians(turns * std::f32::consts::TAU);
        // A 270° arc reads clearly as "busy".
        let end = start + Radians(1.5 * std::f32::consts::PI);

        let path = Path::new(|b| {
            b.arc(canvas::path::Arc {
                center,
                radius,
                start_angle: start,
                end_angle: end,
            });
        });

        frame.stroke(
            &path,
            Stroke::default()
                .with_width(3.5)
                .with_color(theme::tokens(app_theme).accent),
        );

        vec![frame.into_geometry()]
    }
}

/// A spinner whose angle reflects how long the load has been pending.
pub fn spinner<'a, Message: 'a>(elapsed: Duration) -> Element<'a, Message> {
    Canvas::new(Arc { elapsed })
        .width(Length::Fixed(SIZE))
        .height(Length::Fixed(SIZE))
        .into()
}
