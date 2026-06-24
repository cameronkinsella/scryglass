//! Checkerboard backdrop drawn behind transparent images.

use std::cell::Cell;

use iced::mouse;
use iced::widget::canvas::{self, Canvas};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use crate::ui::theme;

/// Edge length of one checker square.
const SQUARE: f32 = 12.0;

struct Board;

#[derive(Default)]
struct BoardState {
    cache: canvas::Cache,
    color: Cell<Option<Color>>,
}

impl<Message> canvas::Program<Message> for Board {
    type State = BoardState;

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        app_theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let light = theme::tokens(app_theme).bg_surface;
        // The cache keys on size, not theme, so drop it when the color changes.
        if state.color.get() != Some(light) {
            state.cache.clear();
            state.color.set(Some(light));
        }
        let geometry = state.cache.draw(renderer, bounds.size(), |frame| {
            let columns = (bounds.width / SQUARE).ceil() as i32;
            let rows = (bounds.height / SQUARE).ceil() as i32;
            for row in 0..rows {
                for column in 0..columns {
                    if (row + column) % 2 == 0 {
                        frame.fill_rectangle(
                            Point::new(column as f32 * SQUARE, row as f32 * SQUARE),
                            Size::new(SQUARE, SQUARE),
                            light,
                        );
                    }
                }
            }
        });
        vec![geometry]
    }
}

/// A full-size checkerboard layer (sits behind the image).
pub fn checkerboard<'a, Message: 'a>() -> Element<'a, Message> {
    Canvas::new(Board)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
