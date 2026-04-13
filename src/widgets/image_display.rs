//! Image display widget: centered image with aspect-ratio preservation.

use iced::widget::{center, container, image, text};
use iced::{Element, Length};

use crate::app::Message;

/// Render the current image from a pre-allocated GPU texture.
pub fn image_display(allocation: &iced::widget::image::Allocation) -> Element<'_, Message> {
    container(
        image(allocation.handle().clone())
            .content_fit(iced::ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .into()
}

/// Render the empty/waiting state drop prompt.
pub fn drop_prompt<'a>() -> Element<'a, Message> {
    center(text("Drop an image here to begin").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render a loading indicator.
pub fn loading_prompt<'a>() -> Element<'a, Message> {
    center(text("Loading…").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
