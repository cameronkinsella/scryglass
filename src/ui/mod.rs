//! Shared visual primitives and formatting helpers.

pub mod checkerboard;
pub mod icons;
pub mod image_display;
pub mod spinner;
pub mod theme;
#[cfg(feature = "video")]
pub mod video_surface;

use iced::{Element, Length};

/// Present `card` as a modal overlay: an opaque backdrop that swallows clicks,
/// the card centered and scrollable, with a close X pinned to the top-right
/// corner. The X stays put while the card scrolls.
pub fn overlay_card<'a, M: Clone + 'a>(
    card: impl Into<Element<'a, M>>,
    on_close: M,
) -> Element<'a, M> {
    use iced::widget::{Stack, button, center, container, opaque, scrollable};

    let close = button(icons::x_lg().size(14))
        .on_press(on_close)
        .padding(5)
        .style(theme::close_button);

    // Stack sizes to its base layer, so the Fill close strip pins to the
    // card's corner without widening it.
    opaque(
        center(Stack::with_children(vec![
            scrollable(card.into()).into(),
            container(close)
                .width(Length::Fill)
                .align_x(iced::Alignment::End)
                .padding(8)
                .into(),
        ]))
        .width(Length::Fill)
        .height(Length::Fill),
    )
}

/// Format image dimensions for display (e.g. "256 × 512 pixels").
pub fn format_dimensions(width: u32, height: u32) -> String {
    format!("{width} × {height} pixels")
}

/// Format a byte count into a human-readable size string.
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// File-size text for display: "…" while the async probe is pending.
pub fn file_size_label(size: Option<u64>) -> String {
    size.map(format_file_size)
        .unwrap_or_else(|| "…".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_file_size_covers_all_units() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(1023), "1023 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(format_file_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn file_size_label_shows_ellipsis_while_pending() {
        assert_eq!(file_size_label(None), "…");
        assert_eq!(file_size_label(Some(2048)), "2.0 KB");
    }
}
