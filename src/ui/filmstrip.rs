//! Filmstrip widget: horizontal strip of image thumbnails for navigation.
//!
//! Uses `Handle::from_path()` directly (no `image::allocate()`). Iced decodes
//! and caches thumbnails internally. Each thumbnail is wrapped in a button
//! that emits a navigation message on click. The current image gets a
//! highlighted border, hovered thumbnails get a different highlight.

use std::path::Path;

use iced::widget::button::{self, Status, Style};
use iced::widget::{container, image, mouse_area, row, scrollable};
use iced::{Background, Border, Color, Element, Length, Padding, Theme};

use crate::app::Message;

/// Thumbnail size in logical pixels.
const THUMB_SIZE: f32 = 60.0;

/// Button style for the current (selected) thumbnail, bright thick border.
fn current_thumb_style(_theme: &Theme, status: Status) -> Style {
    let highlight = Color::from_rgb(0.2, 0.6, 1.0); // vivid blue
    let border = Border {
        color: highlight,
        width: 4.0,
        radius: 4.0.into(),
    };
    match status {
        Status::Hovered | Status::Pressed => Style {
            background: Some(Background::Color(Color::from_rgba(0.2, 0.6, 1.0, 0.3))),
            border,
            ..Style::default()
        },
        _ => Style {
            background: Some(Background::Color(Color::from_rgba(0.2, 0.6, 1.0, 0.1))),
            border,
            ..Style::default()
        },
    }
}

/// Button style for non-current thumbnails, visible hover highlight.
fn thumb_style(_theme: &Theme, status: Status) -> Style {
    match status {
        Status::Hovered => Style {
            background: Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.2))),
            border: Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.7),
                width: 3.0,
                radius: 4.0.into(),
            },
            ..Style::default()
        },
        Status::Pressed => Style {
            background: Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.3))),
            border: Border {
                color: Color::WHITE,
                width: 3.0,
                radius: 4.0.into(),
            },
            ..Style::default()
        },
        _ => Style {
            background: None,
            border: Border {
                color: Color::TRANSPARENT,
                width: 3.0,
                radius: 4.0.into(),
            },
            ..Style::default()
        },
    }
}

/// Render the filmstrip: a horizontal scrollable row of image thumbnails.
pub fn filmstrip<'a>(files: &[impl AsRef<Path>], cursor: usize) -> Element<'a, Message> {
    let thumbs: Vec<Element<'a, Message>> = files
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let handle = iced::widget::image::Handle::from_path(path.as_ref());
            let img = image(handle)
                .content_fit(iced::ContentFit::Cover)
                .width(Length::Fixed(THUMB_SIZE))
                .height(Length::Fixed(THUMB_SIZE));

            let style_fn: fn(&Theme, Status) -> Style = if i == cursor {
                current_thumb_style
            } else {
                thumb_style
            };

            button::Button::new(img)
                .on_press(Message::FilmstripClicked(i))
                .padding(2)
                .style(style_fn)
                .into()
        })
        .collect();

    let strip = row(thumbs).spacing(2);

    let scroll = scrollable(strip)
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(4).scroller_width(4),
        ))
        .id(filmstrip_id())
        .width(Length::Fill);

    // Wrap in mouse_area to intercept vertical scroll and convert to horizontal.
    let scrollable_area = mouse_area(scroll).on_scroll(|delta| {
        let y = match delta {
            iced::mouse::ScrollDelta::Lines { y, .. } => y,
            iced::mouse::ScrollDelta::Pixels { y, .. } => y / 60.0,
        };
        Message::FilmstripScroll(y)
    });

    container(scrollable_area)
        .width(Length::Fill)
        .padding(Padding {
            top: 2.0,
            right: 4.0,
            bottom: 2.0,
            left: 4.0,
        })
        .into()
}

/// Stable widget ID for the filmstrip scrollable (for programmatic scrolling).
pub fn filmstrip_id() -> iced::widget::Id {
    iced::widget::Id::new("filmstrip_scroll")
}
