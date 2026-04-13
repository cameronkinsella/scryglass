//! Image display widget: centered image with zoom and pan support.
//!
//! The image is rendered at `native_size × zoom` using `ContentFit::Fill`,
//! which stretches the image to exactly the requested pixel dimensions.
//! A clipping viewport container ensures only the visible portion is shown.
//! The visible region is cropped from the source and positioned via padding.

use iced::widget::{center, container, image, text};
use iced::{Element, Length, Padding};

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
    let display_w = size.width as f32 * zoom;
    let display_h = size.height as f32 * zoom;

    let vp_w = viewport.0;
    let vp_h = viewport.1;

    // Calculate where the image top-left should be so that the image is
    // centered in the viewport, adjusted by the pan offset.
    let image_x = (vp_w - display_w) / 2.0 + pan.0;
    let image_y = (vp_h - display_h) / 2.0 + pan.1;

    // How much of the image is off-screen on each edge (in display pixels).
    let crop_left = (-image_x).max(0.0);
    let crop_top = (-image_y).max(0.0);
    let crop_right = (image_x + display_w - vp_w).max(0.0);
    let crop_bottom = (image_y + display_h - vp_h).max(0.0);

    // Convert crop amounts from display pixels back to original image pixels.
    let crop_left_px = (crop_left / zoom).round() as u32;
    let crop_top_px = (crop_top / zoom).round() as u32;
    let crop_right_px = (crop_right / zoom).round() as u32;
    let crop_bottom_px = (crop_bottom / zoom).round() as u32;

    let visible_w = size
        .width
        .saturating_sub(crop_left_px + crop_right_px)
        .max(1);
    let visible_h = size
        .height
        .saturating_sub(crop_top_px + crop_bottom_px)
        .max(1);

    let cropped_display_w = visible_w as f32 * zoom;
    let cropped_display_h = visible_h as f32 * zoom;

    // Crop the source image and render only the visible region at zoom scale.
    let cropped_img = image(allocation.handle().clone())
        .content_fit(iced::ContentFit::Fill)
        .width(Length::Fixed(cropped_display_w))
        .height(Length::Fixed(cropped_display_h))
        .crop(iced::Rectangle {
            x: crop_left_px,
            y: crop_top_px,
            width: visible_w,
            height: visible_h,
        });

    // Position: left/top padding is the non-negative part of image_x/y.
    let pad_left = image_x.max(0.0);
    let pad_top = image_y.max(0.0);

    let positioned = container(cropped_img)
        .width(Length::Shrink)
        .height(Length::Shrink)
        .padding(Padding {
            top: pad_top,
            right: 0.0,
            bottom: 0.0,
            left: pad_left,
        });

    container(positioned)
        .width(Length::Fill)
        .height(Length::Fill)
        .clip(true)
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
