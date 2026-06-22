//! Navigation slider for direct cursor positioning, plus the scrub
//! preview bubble for files that can't be shown live mid-drag.

use std::path::PathBuf;

use iced::widget::{container, image, row, slider, space, text};
use iced::{Alignment, Element, Length, Padding, Size};

use crate::app::state::Thumb;
use crate::app::{Message, SliderMessage};
use crate::media::cache::ImageCache;
use crate::ui::theme;

/// Thumbnail edge length inside the scrub bubble.
const BUBBLE_THUMB: f32 = 72.0;

/// Approximate rendered width of the bubble (thumb + label + padding),
/// used to keep it inside the window.
const BUBBLE_WIDTH: f32 = 150.0;

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

/// The scrub preview bubble: floats above the slider at the drag position,
/// showing the target's thumbnail (or a quiet placeholder square) and its
/// position in the directory.
pub fn scrub_bubble<'a>(
    files: &'a [PathBuf],
    target: usize,
    thumbs: &'a ImageCache<Thumb>,
    window: Size,
    show_footer: bool,
) -> Element<'a, Message> {
    let target = target.min(files.len().saturating_sub(1));
    let path = &files[target];

    let pic: Element<'a, Message> = match thumbs.peek(path) {
        Some(thumb) => image(thumb.handle.clone())
            .content_fit(iced::ContentFit::Cover)
            .width(Length::Fixed(BUBBLE_THUMB))
            .height(Length::Fixed(BUBBLE_THUMB))
            .into(),
        None => container(space::horizontal())
            .width(Length::Fixed(BUBBLE_THUMB))
            .height(Length::Fixed(BUBBLE_THUMB))
            .style(theme::thumb_placeholder)
            .into(),
    };

    let label = text(format!("{}/{}", target + 1, files.len())).size(12);

    let card = container(
        row![pic, label]
            .spacing(8)
            .align_y(Alignment::Center)
            .padding(6),
    )
    .style(theme::panel);

    // Center the bubble over the slider fraction, clamped to the window.
    let fraction = if files.len() > 1 {
        target as f32 / (files.len() - 1) as f32
    } else {
        0.0
    };
    let left = (fraction * window.width - BUBBLE_WIDTH / 2.0)
        .clamp(4.0, (window.width - BUBBLE_WIDTH - 4.0).max(4.0));
    let bottom = if show_footer { 25.0 } else { 0.0 } + 28.0 + 6.0;

    container(card)
        .align_y(Alignment::End)
        .padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom,
            left,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::{nav_slider, scrub_bubble};
    use crate::app::test_support::{cache_thumb, viewing_app};
    use iced::Size;
    use iced_test::simulator;
    use std::path::PathBuf;

    #[test]
    fn nav_slider_builds_for_single_and_many() {
        let _ = nav_slider(0, 1);
        let _ = nav_slider(3, 10);
    }

    #[test]
    fn scrub_bubble_shows_position_with_and_without_a_thumb() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        cache_thumb(&mut app, "a.png", 8, 8);
        let files: Vec<PathBuf> = ["a.png", "b.png", "c.png"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let viewer = app.viewer().unwrap();
        let window = Size::new(800.0, 600.0);

        let mut cached = simulator(scrub_bubble(&files, 0, &viewer.thumbs, window, true));
        assert!(cached.find("1/3").is_ok());

        let mut uncached = simulator(scrub_bubble(&files, 1, &viewer.thumbs, window, false));
        assert!(uncached.find("2/3").is_ok());
    }
}
