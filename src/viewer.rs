//! Image display widget: renders the current image centered with aspect-ratio preservation.

use iced::widget::image::Allocation;
use iced::widget::{center, image, text};
use iced::{Element, Length};

use crate::app::Message;

/// Render the current image from a pre-allocated GPU texture.
pub fn image_viewer(allocation: &Allocation) -> Element<'_, Message> {
    center(
        image(allocation.handle().clone())
            .content_fit(iced::ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Render the empty/waiting state drop prompt.
pub fn drop_prompt<'a>() -> Element<'a, Message> {
    center(text("Drop an image here to begin").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render a loading prompt while the first image is being allocated.
pub fn loading_prompt<'a>() -> Element<'a, Message> {
    center(text("Loading…").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
