//! Navigation slider for direct cursor positioning.

use iced::widget::{container, slider};
use iced::{Element, Length, Padding};

use crate::app::SliderMessage;

/// Render the navigation slider spanning full width.
///
/// `value` is the position to render the thumb at: the drag target while
/// scrubbing, the cursor otherwise.
pub fn nav_slider<'a>(value: usize, len: usize) -> Element<'a, SliderMessage> {
    let max = if len > 1 { (len - 1) as u32 } else { 0 };

    let s = slider(0..=max, value as u32, |v| {
        SliderMessage::Changed(v as usize)
    })
    .on_release(SliderMessage::Released)
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

#[cfg(test)]
mod tests {
    use super::nav_slider;

    #[test]
    fn nav_slider_builds_for_single_and_many() {
        let _ = nav_slider(0, 1);
        let _ = nav_slider(3, 10);
    }
}
