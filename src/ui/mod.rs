//! UI widgets: toolbar, image display, footer, filmstrip, slider, and context menu.

pub mod context_menu;
pub mod filmstrip;
pub mod footer;
pub mod image_display;
pub mod nav_slider;
pub mod theme;
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
