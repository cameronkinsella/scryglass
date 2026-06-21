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

use crate::app::FilmstripMessage;
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

/// Padding on each side of the strip (the container inset). The scroll math
/// runs in the real scrollable width, not the full window.
const SIDE_INSET: f32 = 4.0;

/// The scrollable's real width: the viewport minus both side insets.
fn usable_width(viewport_w: f32) -> f32 {
    (viewport_w - 2.0 * SIDE_INSET).max(0.0)
}

/// Room kept below the cells for the horizontal scrollbar, so it doesn't
/// cover the cursor cell's bottom border.
const SCROLLBAR_CLEARANCE: f32 = 6.0;

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
    (index as f32 * STRIDE - (usable_width(viewport_w) - STRIDE) / 2.0).max(0.0)
}

/// Furthest the strip can scroll: content width past the usable width, or
/// zero when everything fits.
fn max_scroll(len: usize, viewport_w: f32) -> f32 {
    (len as f32 * STRIDE - usable_width(viewport_w)).max(0.0)
}

/// Scroll offset for opening at `cursor`: stay at the left if the cursor's
/// cell already fits there, otherwise center it, never past the end (so a
/// near-end open lands flush right).
pub fn open_offset(cursor: usize, viewport_w: f32, len: usize) -> f32 {
    if (cursor as f32 + 1.0) * STRIDE <= usable_width(viewport_w) {
        0.0
    } else {
        centering_offset(cursor, viewport_w).min(max_scroll(len, viewport_w))
    }
}

/// Smallest change from `scroll_x` that keeps `cursor`'s cell fully on
/// screen: unchanged if it already is, otherwise its near edge pinned to the
/// matching viewport edge.
pub fn keep_visible_offset(scroll_x: f32, cursor: usize, viewport_w: f32, len: usize) -> f32 {
    let usable = usable_width(viewport_w);
    let cell_start = cursor as f32 * STRIDE;
    let cell_end = cell_start + STRIDE;
    let moved = if cell_start < scroll_x {
        cell_start
    } else if cell_end > scroll_x + usable {
        cell_end - usable
    } else {
        scroll_x
    };
    moved.clamp(0.0, max_scroll(len, viewport_w))
}

/// Render the filmstrip: a virtualized, horizontally scrollable thumbnail row.
pub fn filmstrip<'a>(
    files: &'a [PathBuf],
    cursor: usize,
    thumbs: &'a ImageCache<Thumb>,
    scroll_x: f32,
    viewport_w: f32,
) -> Element<'a, FilmstripMessage> {
    let range = visible_range(scroll_x, viewport_w, files.len());

    let mut cells: Vec<Element<'a, FilmstripMessage>> = Vec::with_capacity(range.len() + 3);

    // Center the strip when the thumbnails don't fill it.
    let content_w = files.len() as f32 * STRIDE;
    let inner_w = usable_width(viewport_w);
    if content_w < inner_w {
        cells.push(
            space::horizontal()
                .width(Length::Fixed((inner_w - content_w) / 2.0))
                .into(),
        );
    }

    if range.start > 0 {
        cells.push(
            space::horizontal()
                .width(Length::Fixed(range.start as f32 * STRIDE))
                .into(),
        );
    }

    for (i, path) in files[range.clone()].iter().enumerate() {
        let index = range.start + i;

        let content: Element<'a, FilmstripMessage> = match thumbs.peek(path) {
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
            .on_press(FilmstripMessage::Clicked(index))
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

    // Reserve room below the cells for the scrollbar when it shows, so it
    // doesn't clip the cursor cell's bottom border.
    let bottom = if content_w > inner_w {
        SCROLLBAR_CLEARANCE
    } else {
        0.0
    };
    let strip = container(row(cells)).padding(Padding {
        bottom,
        ..Padding::ZERO
    });

    let scroll = scrollable(strip)
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(4).scroller_width(4),
        ))
        .id(filmstrip_id())
        .on_scroll(|viewport| FilmstripMessage::Scrolled(viewport.absolute_offset().x))
        .width(Length::Fill);

    // Wrap in mouse_area to intercept vertical scroll and convert to horizontal.
    let scrollable_area = mouse_area(scroll).on_scroll(|delta| {
        let y = match delta {
            iced::mouse::ScrollDelta::Lines { y, .. } => y,
            iced::mouse::ScrollDelta::Pixels { y, .. } => y / 60.0,
        };
        FilmstripMessage::Scroll(y)
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
        // Centered: the cell midpoint sits at the usable-width midpoint.
        assert_eq!(
            offset,
            100.0 * STRIDE - (usable_width(800.0) - STRIDE) / 2.0
        );
    }

    #[test]
    fn max_scroll_is_zero_when_everything_fits() {
        assert_eq!(max_scroll(5, 800.0), 0.0);
        assert_eq!(max_scroll(20, 800.0), 20.0 * STRIDE - usable_width(800.0));
    }

    #[test]
    fn open_offset_stays_left_when_the_cursor_fits() {
        assert_eq!(open_offset(3, 800.0, 1000), 0.0);
    }

    #[test]
    fn open_offset_centers_a_deep_cursor() {
        assert_eq!(open_offset(100, 800.0, 1000), centering_offset(100, 800.0));
    }

    #[test]
    fn open_offset_lands_flush_right_near_the_end() {
        let len = 50;
        assert_eq!(open_offset(len - 1, 800.0, len), max_scroll(len, 800.0));
    }

    #[test]
    fn keep_visible_holds_when_the_cursor_is_on_screen() {
        let scroll = 10.0 * STRIDE;
        assert_eq!(keep_visible_offset(scroll, 12, 800.0, 1000), scroll);
    }

    #[test]
    fn keep_visible_pins_to_the_right_edge() {
        let cursor = 13; // cell ends past the usable width
        assert_eq!(
            keep_visible_offset(0.0, cursor, 800.0, 1000),
            (cursor as f32 + 1.0) * STRIDE - usable_width(800.0)
        );
    }

    #[test]
    fn keep_visible_pins_to_the_left_edge() {
        let scroll = 20.0 * STRIDE;
        assert_eq!(keep_visible_offset(scroll, 15, 800.0, 1000), 15.0 * STRIDE);
    }

    #[test]
    fn keep_visible_clamps_to_the_end() {
        // A tiny list never scrolls, even if asked.
        assert_eq!(keep_visible_offset(500.0, 2, 800.0, 4), 0.0);
    }

    #[test]
    fn keep_visible_keeps_the_last_cell_fully_on_screen() {
        // Pinning the final cursor right must not overshoot the usable width,
        // or the cursor's right border is clipped.
        let len = 50;
        let scroll = keep_visible_offset(0.0, len - 1, 800.0, len);
        let cell_end = len as f32 * STRIDE;
        assert!(cell_end <= scroll + usable_width(800.0) + 0.01);
    }
}
