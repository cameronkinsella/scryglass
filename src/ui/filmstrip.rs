//! Filmstrip widget: horizontal strip of image thumbnails for navigation.
//!
//! Uses `Handle::from_path()` directly (no `image::allocate()`). Iced decodes
//! and caches thumbnails internally. Each thumbnail is wrapped in a button
//! that emits a navigation message on click. The current image gets a
//! highlighted border, hovered thumbnails get a different highlight.

use std::path::Path;

use iced::widget::{button, container, image, mouse_area, row, scrollable};
use iced::{Element, Length, Padding};

use crate::app::Message;
use crate::ui::theme;

/// Thumbnail size in logical pixels.
const THUMB_SIZE: f32 = 60.0;

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

            button(img)
                .on_press(Message::FilmstripClicked(i))
                .padding(2)
                .style(if i == cursor {
                    theme::thumb_current
                } else {
                    theme::thumb
                })
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
