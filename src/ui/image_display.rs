//! Blur widget for the pending state: an iced `Image` that renders a thumbnail
//! into exactly the rect the resident content will fill, plus the drop, empty,
//! and error placeholders for the image area.

use iced::widget::image::{FilterMethod, Handle};
use iced::widget::{center, container, image, text};
use iced::{ContentFit, Element, Length, Rectangle};

use crate::app::Message;
use crate::ui::geometry::display_geometry;

/// Render a thumbnail blur into exactly the rect the resident content will occupy.
///
/// The destination rect and source window come from the same [`display_geometry`]
/// the still and video shaders use, so the swap to full content never resizes or
/// shifts. The thumbnail is `ContentFit::Fill`-stretched into that rect, so its
/// own (rounded) aspect is ignored, which is imperceptible on a blur.
///
/// * `handle`: the thumbnail texture.
/// * `texture_size`: its dimensions, for mapping the source window into it.
/// * `original_size`: the image's true dimensions, the zoom/pan space.
/// * `zoom`: zoom factor (1.0 = 100% of original pixel size).
/// * `pan`: pan offset in logical pixels `(dx, dy)`.
/// * `viewport`: size of the display area in logical pixels `(w, h)`.
/// * `origin`: the display area's top-left in the window, logical pixels.
/// * `pixelated`: nearest sampling when zoomed past 100% (crisp pixel art).
#[allow(clippy::too_many_arguments)]
pub fn image_display(
    handle: &Handle,
    texture_size: (u32, u32),
    original_size: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    origin: (f32, f32),
    pixelated: bool,
) -> Element<'_, Message> {
    let (vp_w, vp_h) = viewport;
    let Some((dst, src)) = display_geometry(zoom, pan, viewport, original_size) else {
        return empty_viewport();
    };
    let filter = if pixelated && zoom > 1.0 {
        FilterMethod::Nearest
    } else {
        FilterMethod::Linear
    };
    // The source window in thumbnail pixels (UV from `display_geometry` times its size).
    let (tw, th) = (texture_size.0 as f32, texture_size.1 as f32);
    let crop = Rectangle {
        x: (src[0] * tw).round() as u32,
        y: (src[1] * th).round() as u32,
        width: (((src[2] - src[0]) * tw).round() as u32).max(1),
        height: (((src[3] - src[1]) * th).round() as u32).max(1),
    };
    // Snap to the same whole-pixel rect the shader gives the sharp image, both size
    // and offset rounded the same way (`snap_placement_to_pixels`), so the swap
    // between blur and sharp never shifts. That rounding happens in FRAMEBUFFER
    // space: the widget's own physical offset in the window is fractional under
    // chrome, so the area's origin rides in and cancels out. `center` computes and
    // floors the offset itself, drifting the blur up and left by a pixel.
    // Positioning it explicitly at the rounded offset leaves iced no fractional
    // value to floor.
    let scale = crate::ui::image_surface::current_scale_factor().max(1.0);
    let axis = |d0: f32, d1: f32, vp: f32, org: f32| {
        let phys = vp * scale;
        let origin = org * scale;
        let pixels = ((d1 - d0) * phys).round().max(1.0);
        let a0 = (origin + (d0 + d1) * 0.5 * phys - pixels * 0.5).round() - origin;
        (a0 / scale, pixels / scale)
    };
    let (left, w) = axis(dst[0], dst[2], vp_w, origin.0);
    let (top, h) = axis(dst[1], dst[3], vp_h, origin.1);
    container(
        image(handle.clone())
            .content_fit(ContentFit::Fill)
            .filter_method(filter)
            .crop(crop)
            .width(Length::Fixed(w))
            .height(Length::Fixed(h)),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(iced::Padding {
        top,
        right: 0.0,
        bottom: 0.0,
        left,
    })
    .into()
}

/// Render the empty/waiting state drop prompt.
pub fn drop_prompt<'a>() -> Element<'a, Message> {
    center(
        text("Drop an image here to begin")
            .size(24)
            .width(Length::Fill)
            .center()
            .style(crate::ui::theme::secondary_text),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The image area with nothing ready for the current file yet. Quiet and
/// honest, never a previous image. The spinner overlay handles feedback.
pub fn empty_viewport<'a>() -> Element<'a, Message> {
    container(text(""))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Shown in the image area for a file that could not be decoded, so a broken
/// file reads as an error rather than a blank, uncrossable gap.
pub fn error_viewport<'a>(message: &str) -> Element<'a, Message> {
    center(
        text(message.to_string())
            .size(16)
            .width(Length::Fill)
            .center()
            .style(crate::ui::theme::secondary_text),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f32, f32) = (800.0, 600.0);

    #[test]
    fn drop_prompt_invites_the_user() {
        use iced_test::simulator;
        let mut ui = simulator(super::drop_prompt());
        assert!(ui.find("Drop an image here to begin").is_ok());
    }

    #[test]
    fn error_viewport_shows_the_message() {
        use iced_test::simulator;
        let mut ui = simulator(super::error_viewport("could not decode image"));
        assert!(ui.find("could not decode image").is_ok());
    }

    #[test]
    fn image_display_builds_fit_crop_and_empty_paths() {
        let handle = Handle::from_rgba(4, 4, vec![0u8; 4 * 4 * 4]);
        let _ = image_display(
            &handle,
            (4, 4),
            (4, 4),
            1.0,
            (0.0, 0.0),
            VP,
            (0.0, 30.5),
            false,
        );
        let _ = image_display(
            &handle,
            (4, 4),
            (4000, 3000),
            5.0,
            (0.0, 0.0),
            VP,
            (0.0, 0.0),
            true,
        );
        let _ = image_display(
            &handle,
            (4, 4),
            (4, 4),
            0.0,
            (0.0, 0.0),
            VP,
            (0.0, 0.0),
            false,
        );
    }
}
