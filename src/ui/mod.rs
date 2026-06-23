//! Shared visual primitives and formatting helpers.

pub mod checkerboard;
pub mod icons;
pub mod image_display;
pub mod spinner;
pub mod theme;
#[cfg(feature = "video")]
pub mod video_surface;

use iced::{Element, Length};

/// Present `content` as a click-away modal: an opaque backdrop that keeps the
/// app behind inert and dismisses on any outside click, the content framed,
/// centered, and scrollable, with a close X pinned to the top-right corner.
pub fn overlay_card<'a, M: Clone + 'a>(
    content: impl Into<Element<'a, M>>,
    on_close: M,
) -> Element<'a, M> {
    use iced::widget::{Stack, button, center, container, mouse_area, opaque, scrollable};

    let close = button(container(icons::x_lg().size(14)).center(Length::Fill))
        .on_press(on_close.clone())
        .width(Length::Fixed(24.0))
        .height(Length::Fixed(24.0))
        .padding(0)
        .style(theme::icon_button);

    // Frame outside the scroll area so its corners stay put as the content scrolls.
    let frame = container(
        scrollable(content.into()).direction(scrollable::Direction::Both {
            vertical: scrollable::Scrollbar::new(),
            horizontal: scrollable::Scrollbar::new(),
        }),
    )
    .style(theme::panel);

    let panel = opaque(Stack::with_children(vec![
        frame.into(),
        container(close)
            .width(Length::Fill)
            .align_x(iced::Alignment::End)
            .padding([6, 16])
            .into(),
    ]));

    opaque(
        mouse_area(
            center(panel)
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(4),
        )
        .on_press(on_close),
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
