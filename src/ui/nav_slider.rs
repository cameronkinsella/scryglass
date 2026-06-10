//! Navigation slider: horizontal slider for direct cursor positioning.

use iced::widget::{container, slider};
use iced::{Element, Length, Padding};

use crate::app::Message;

/// Render the navigation slider spanning full width.
///
/// The slider has discrete steps corresponding to each image index.
pub fn nav_slider<'a>(cursor: usize, len: usize) -> Element<'a, Message> {
    let max = if len > 1 { (len - 1) as u32 } else { 0 };
    let value = cursor as u32;

    let s = slider(0..=max, value, |v| Message::SliderChanged(v as usize))
        .step(1u32)
        .width(Length::Fill)
        .height(24);

    container(s)
        .width(Length::Fill)
        .padding(Padding {
            top: 2.0,
            right: 12.0,
            bottom: 2.0,
            left: 12.0,
        })
        .into()
}
