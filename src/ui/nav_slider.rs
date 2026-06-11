//! Navigation slider: horizontal slider for direct cursor positioning,
//! plus the scrub preview bubble.
//!
//! During a drag the thumb follows the hand freely (the caller passes the
//! drag target as `value`). Navigation commits on release. The bubble is
//! the fallback preview for files that can't be shown live mid-drag.

use std::path::PathBuf;

use iced::widget::{container, image, row, slider, space, text};
use iced::{Alignment, Element, Length, Padding, Size};

use crate::app::Message;
use crate::app::state::Thumb;
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
pub fn nav_slider<'a>(value: usize, len: usize) -> Element<'a, Message> {
    let max = if len > 1 { (len - 1) as u32 } else { 0 };

    let s = slider(0..=max, value as u32, |v| {
        Message::SliderChanged(v as usize)
    })
    .on_release(Message::SliderReleased)
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
