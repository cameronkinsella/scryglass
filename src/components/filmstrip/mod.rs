#[derive(Debug, Clone)]
pub enum Message {
    Clicked(usize),
    Scroll(f32),
    Scrolled(f32),
}
use iced::Element;
use iced::Task;

use crate::app::update::{NavTarget, fire_visible_thumbs, navigate};
use crate::app::{App, Message as AppMessage};

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    let Some(viewer) = app.viewer() else {
        return iced::widget::row![].into();
    };

    widget::filmstrip(
        viewer.nav.files(),
        viewer.nav.cursor(),
        &viewer.thumbs,
        viewer.filmstrip_scroll_x,
        app.window_size.width,
    )
    .map(AppMessage::Filmstrip)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Scroll(delta_y) => {
            // Convert vertical scroll delta to horizontal scroll on the filmstrip.
            let offset = iced::widget::scrollable::AbsoluteOffset {
                x: -delta_y * 60.0,
                y: 0.0,
            };
            iced::widget::operation::scroll_by(widget::filmstrip_id(), offset)
        }

        Message::Scrolled(x) => {
            let window_w = app.window_size.width;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.filmstrip_scroll_x = x;
            Task::batch(fire_visible_thumbs(&pipeline, viewer, window_w))
        }

        Message::Clicked(index) => navigate(app, NavTarget::Index(index)),
    }
}

pub(crate) use widget::{centering_offset, filmstrip_id, visible_range};
mod widget;
