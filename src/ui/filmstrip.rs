//! Filmstrip widget: horizontal strip of image thumbnails for navigation.
//!
//! Virtualized: only the items near the scroll position are materialized;
//! fixed-width spacers stand in for everything off-screen, so a directory
//! of ten thousand files costs the same as a dozen. Thumbnails come from
//! the viewer's thumb cache (filled by decodes and EXIF probes). Files
//! without a cached thumb yet show a quiet placeholder square.

use std::ops::Range;
use std::path::PathBuf;

use iced::widget::{button, container, image, mouse_area, row, scrollable, space};
use iced::{Element, Length, Padding};

use crate::app::Message;
use crate::app::state::Thumb;
use crate::media::cache::ImageCache;
use crate::ui::theme;

/// Thumbnail size in logical pixels.
const THUMB_SIZE: f32 = 60.0;

/// Button padding around each thumbnail.
const ITEM_PADDING: f32 = 2.0;

/// Gap between items.
const SPACING: f32 = 2.0;

/// Horizontal stride of one filmstrip cell (button + gap).
pub const STRIDE: f32 = THUMB_SIZE + 2.0 * ITEM_PADDING + SPACING;

/// Extra items materialized on each side of the visible window, so quick
/// scrolls don't reveal blank cells.
const OVERSCAN: usize = 8;

/// Index range of filmstrip items worth materializing for the given
/// scroll offset and viewport width.
pub fn visible_range(scroll_x: f32, viewport_w: f32, len: usize) -> Range<usize> {
    if len == 0 {
        return 0..0;
    }
    let first = (scroll_x / STRIDE).max(0.0) as usize;
    let count = (viewport_w / STRIDE).ceil() as usize + 1;
    let start = first.saturating_sub(OVERSCAN);
    let end = (first + count + OVERSCAN).min(len);
    start..end
}

/// Scroll offset that horizontally centers `index` in the strip.
pub fn centering_offset(index: usize, viewport_w: f32) -> f32 {
    (index as f32 * STRIDE - (viewport_w - STRIDE) / 2.0).max(0.0)
}

/// Render the filmstrip: a virtualized, horizontally scrollable thumbnail row.
pub fn filmstrip<'a>(
    files: &'a [PathBuf],
    cursor: usize,
    thumbs: &'a ImageCache<Thumb>,
    scroll_x: f32,
    viewport_w: f32,
) -> Element<'a, Message> {
    let range = visible_range(scroll_x, viewport_w, files.len());

    let mut cells: Vec<Element<'a, Message>> = Vec::with_capacity(range.len() + 2);

    if range.start > 0 {
        cells.push(
            space::horizontal()
                .width(Length::Fixed(range.start as f32 * STRIDE))
                .into(),
        );
    }

    for (i, path) in files[range.clone()].iter().enumerate() {
        let index = range.start + i;

        let content: Element<'a, Message> = match thumbs.peek(path) {
            Some(thumb) => image(thumb.handle.clone())
                .content_fit(iced::ContentFit::Cover)
                .width(Length::Fixed(THUMB_SIZE))
                .height(Length::Fixed(THUMB_SIZE))
                .into(),
            None => container(space::horizontal())
                .width(Length::Fixed(THUMB_SIZE))
                .height(Length::Fixed(THUMB_SIZE))
                .style(theme::thumb_placeholder)
                .into(),
        };

        let cell = button(content)
            .on_press(Message::FilmstripClicked(index))
            .padding(ITEM_PADDING)
            .style(if index == cursor {
                theme::thumb_current
            } else {
                theme::thumb
            });

        // The gap is baked into each cell so item positions stay exactly
        // index * STRIDE. Virtualization depends on it.
        cells.push(
            container(cell)
                .padding(Padding {
                    top: 0.0,
                    right: SPACING,
                    bottom: 0.0,
                    left: 0.0,
                })
                .into(),
        );
    }

    if range.end < files.len() {
        cells.push(
            space::horizontal()
                .width(Length::Fixed((files.len() - range.end) as f32 * STRIDE))
                .into(),
        );
    }

    let strip = row(cells);

    let scroll = scrollable(strip)
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(4).scroller_width(4),
        ))
        .id(filmstrip_id())
        .on_scroll(|viewport| Message::FilmstripScrolled(viewport.absolute_offset().x))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_at_origin_covers_first_screen_plus_overscan() {
        let range = visible_range(0.0, 800.0, 1000);
        assert_eq!(range.start, 0);
        // ceil(800 / 66) + 1 = 14 items on screen, plus trailing overscan.
        assert_eq!(range.end, 14 + OVERSCAN);
    }

    #[test]
    fn visible_range_scrolled_includes_overscan_on_both_sides() {
        let scroll = 20.0 * STRIDE;
        let range = visible_range(scroll, 660.0, 1000);
        assert_eq!(range.start, 20 - OVERSCAN);
        // 11 on screen
        assert_eq!(range.end, 20 + 11 + OVERSCAN);
    }

    #[test]
    fn visible_range_clamps_to_len() {
        let range = visible_range(0.0, 10_000.0, 5);
        assert_eq!(range, 0..5);
        assert_eq!(visible_range(0.0, 800.0, 0), 0..0);
    }

    #[test]
    fn centering_offset_clamps_at_start() {
        assert_eq!(centering_offset(0, 800.0), 0.0);
        assert_eq!(centering_offset(1, 800.0), 0.0);
    }

    #[test]
    fn centering_offset_centers_middle_items() {
        let offset = centering_offset(100, 800.0);
        // Item 100 starts at 6600, centered means its cell midpoint sits at
        // viewport midpoint: 6600 - (800 - 66) / 2 = 6233.
        assert_eq!(offset, 100.0 * STRIDE - (800.0 - STRIDE) / 2.0);
    }
}
