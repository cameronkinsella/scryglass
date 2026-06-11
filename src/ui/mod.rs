//! UI widgets: toolbar, image display, footer, filmstrip, slider, and context menu.

pub mod checkerboard;
pub mod context_menu;
pub mod dialogs;
pub mod filmstrip;
pub mod footer;
pub mod help;
pub mod icons;
pub mod image_display;
pub mod info_panel;
pub mod nav_slider;
pub mod shortcuts;
pub mod spinner;
pub mod theme;
pub mod toast;
pub mod toolbar;

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
