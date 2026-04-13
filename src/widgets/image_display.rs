//! Image display widget: centered image with zoom and pan support.
//!
//! Zoom and pan are implemented via the `crop()` method on the iced
//! `Image` widget. At any given zoom level the "visible window" in
//! source-pixel space is `(viewport / zoom)`. The pan offset shifts
//! that window. The cropped region is then displayed with
//! `ContentFit::Contain` inside a `Length::Fill` layout, so iced
//! scales it up to fill the viewport, effectively rendering the
//! image at the desired zoom level.
//!
//! This approach avoids `scale()` entirely, which eliminates the
//! interaction between scale-from-center and container positioning
//! that caused the old padding-based pan to resize the image.

use iced::widget::{center, container, image, text};
use iced::{ContentFit, Element, Length, Rectangle};

use crate::app::Message;

/// Render the current image from a pre-allocated GPU texture.
///
/// * `zoom`: zoom factor (1.0 = 100% of native pixel size).
/// * `pan`: pan offset in logical pixels `(dx, dy)`.
///   Positive X shifts the image right, positive Y shifts it down.
/// * `viewport`: size of the display area in logical pixels `(w, h)`.
pub fn image_display(
    allocation: &iced::widget::image::Allocation,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
) -> Element<'_, Message> {
    let size = allocation.size();
    let img_w = size.width as f32;
    let img_h = size.height as f32;
    let vp_w = viewport.0;
    let vp_h = viewport.1;

    if img_w <= 0.0 || img_h <= 0.0 || zoom <= 0.0 {
        return container(text(""))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    // The zoomed image size in logical pixels.
    let zoomed_w = img_w * zoom;
    let zoomed_h = img_h * zoom;

    // If the zoomed image fits entirely within the viewport, no cropping
    // is needed. Just show the whole image centered, scaled down.
    if zoomed_w <= vp_w && zoomed_h <= vp_h {
        // ContentFit::Contain with Fill layout scales the image to fit the
        // viewport (contain_zoom). We want the image at `zoom` size, so
        // apply scale = zoom / contain_zoom.
        let contain_zoom = (vp_w / img_w).min(vp_h / img_h);
        let scale_factor = if contain_zoom > 0.0 {
            zoom / contain_zoom
        } else {
            1.0
        };

        let img_widget = image(allocation.handle().clone())
            .content_fit(ContentFit::Contain)
            .width(Length::Fill)
            .height(Length::Fill)
            .scale(scale_factor);

        return container(img_widget)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    // --- Crop-based zoom & pan ---
    //
    // The visible window in source pixels: viewport / zoom.
    // Clamp to image dimensions so we never request more than exists.
    let view_src_w = (vp_w / zoom).min(img_w);
    let view_src_h = (vp_h / zoom).min(img_h);

    // Center of the visible window in source pixels.
    // Default center is the image center, pan shifts it.
    // Pan is in logical (screen) pixels, so convert to source pixels
    // by dividing by zoom.
    let cx = img_w / 2.0 - pan.0 / zoom;
    let cy = img_h / 2.0 - pan.1 / zoom;

    // Top-left corner of the crop rectangle, clamped to valid range.
    let crop_x = (cx - view_src_w / 2.0).clamp(0.0, img_w - view_src_w);
    let crop_y = (cy - view_src_h / 2.0).clamp(0.0, img_h - view_src_h);

    let crop_rect = Rectangle {
        x: crop_x.round() as u32,
        y: crop_y.round() as u32,
        width: view_src_w.round().max(1.0) as u32,
        height: view_src_h.round().max(1.0) as u32,
    };

    let img_widget = image(allocation.handle().clone())
        .content_fit(ContentFit::Contain)
        .width(Length::Fill)
        .height(Length::Fill)
        .crop(crop_rect);

    container(img_widget)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the empty/waiting state drop prompt.
pub fn drop_prompt<'a>() -> Element<'a, Message> {
    center(text("Drop an image here to begin").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render a loading indicator.
pub fn loading_prompt<'a>() -> Element<'a, Message> {
    center(text("Loading…").size(24))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
