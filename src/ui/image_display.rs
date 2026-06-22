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
//! Zoom and pan operate in *original* pixel space (the image's true
//! dimensions), while the texture may be a downscaled version of a huge
//! image. The crop rectangle is mapped from original space into texture
//! space at the end, so the same math drives full-resolution images,
//! capped giants, and (later) low-res placeholders identically.

use iced::widget::image::{FilterMethod, Handle};
use iced::widget::{center, container, image, text};
use iced::{ContentFit, Element, Length, Rectangle};

use crate::app::Message;

/// How to render the texture for a given zoom/pan state.
#[derive(Debug, PartialEq)]
pub(crate) enum DisplayMath {
    /// Image is invalid/degenerate, render nothing.
    Empty,
    /// The zoomed image fits in the viewport: scale the whole texture.
    Fit { scale_factor: f32 },
    /// The zoomed image overflows: crop a window of the texture.
    Crop { rect: Rectangle<u32> },
}

/// Pure display math: decides between fit and crop and computes the
/// numbers, mapping from original-pixel space to texture space.
pub(crate) fn display_math(
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    original: (u32, u32),
    texture: (u32, u32),
) -> DisplayMath {
    let img_w = original.0 as f32;
    let img_h = original.1 as f32;
    let tex_w = texture.0 as f32;
    let tex_h = texture.1 as f32;
    let (vp_w, vp_h) = viewport;

    if img_w <= 0.0 || img_h <= 0.0 || tex_w <= 0.0 || tex_h <= 0.0 || zoom <= 0.0 {
        return DisplayMath::Empty;
    }

    // The zoomed image size in logical pixels (original space).
    let zoomed_w = img_w * zoom;
    let zoomed_h = img_h * zoom;

    // The zoomed image fits the viewport: no crop, just scale the whole
    // texture to the zoomed size.
    if zoomed_w <= vp_w && zoomed_h <= vp_h {
        // ContentFit::Contain in a Fill layout shows the texture at
        // min(vp/tex) scale, so correct it to hit the target size.
        let contain = (vp_w / tex_w).min(vp_h / tex_h);
        let shown_w = tex_w * contain;
        let scale_factor = if shown_w > 0.0 {
            zoomed_w / shown_w
        } else {
            1.0
        };
        return DisplayMath::Fit { scale_factor };
    }

    // --- Crop-based zoom & pan (in original space) ---
    //
    // The visible window in source pixels: viewport / zoom.
    let view_src_w = (vp_w / zoom).min(img_w);
    let view_src_h = (vp_h / zoom).min(img_h);

    // Center of the visible window. Pan is in logical (screen) pixels,
    // so convert to source pixels by dividing by zoom.
    let cx = img_w / 2.0 - pan.0 / zoom;
    let cy = img_h / 2.0 - pan.1 / zoom;

    // Top-left corner of the crop rectangle, clamped to valid range.
    let crop_x = (cx - view_src_w / 2.0).clamp(0.0, img_w - view_src_w);
    let crop_y = (cy - view_src_h / 2.0).clamp(0.0, img_h - view_src_h);

    // Map from original space into texture space.
    let sx = tex_w / img_w;
    let sy = tex_h / img_h;

    DisplayMath::Crop {
        rect: Rectangle {
            x: (crop_x * sx).round() as u32,
            y: (crop_y * sy).round() as u32,
            width: ((view_src_w * sx).round() as u32).max(1),
            height: ((view_src_h * sy).round() as u32).max(1),
        },
    }
}

/// Render an image texture at the given zoom/pan.
///
/// * `handle`: the GPU texture (possibly a downscaled version).
/// * `texture_size`: dimensions of that texture.
/// * `original_size`: the image's true dimensions, the zoom/pan space.
/// * `zoom`: zoom factor (1.0 = 100% of original pixel size).
/// * `pan`: pan offset in logical pixels `(dx, dy)`.
/// * `viewport`: size of the display area in logical pixels `(w, h)`.
/// * `pixelated`: nearest-neighbor sampling when zoomed past 100%
///   (crisp pixel art). Downscales always use linear filtering.
pub fn image_display(
    handle: &Handle,
    texture_size: (u32, u32),
    original_size: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
) -> Element<'_, Message> {
    let filter = if pixelated && zoom > 1.0 {
        FilterMethod::Nearest
    } else {
        FilterMethod::Linear
    };

    let widget: Element<'_, Message> =
        match display_math(zoom, pan, viewport, original_size, texture_size) {
            DisplayMath::Empty => text("").into(),
            DisplayMath::Fit { scale_factor } => image(handle.clone())
                .content_fit(ContentFit::Contain)
                .filter_method(filter)
                .width(Length::Fill)
                .height(Length::Fill)
                .scale(scale_factor)
                .into(),
            DisplayMath::Crop { rect } => image(handle.clone())
                .content_fit(ContentFit::Contain)
                .filter_method(filter)
                .width(Length::Fill)
                .height(Length::Fill)
                .crop(rect)
                .into(),
        };

    container(widget)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Render the empty/waiting state drop prompt.
pub fn drop_prompt<'a>() -> Element<'a, Message> {
    center(
        text("Drop an image here to begin")
            .size(24)
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

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f32, f32) = (800.0, 600.0);

    #[test]
    fn small_image_at_full_zoom_uses_fit() {
        // 400×300 at 100% in 800×600: contain factor = 2.0, so the target
        // on-screen width is 400 → scale_factor = 0.5.
        let math = display_math(1.0, (0.0, 0.0), VP, (400, 300), (400, 300));
        assert_eq!(math, DisplayMath::Fit { scale_factor: 0.5 });
    }

    #[test]
    fn fit_scale_is_resolution_independent() {
        // A half-resolution texture of the same image must produce the
        // same on-screen size: contain factor doubles, scale halves out.
        let full = display_math(1.0, (0.0, 0.0), VP, (400, 300), (400, 300));
        let half = display_math(1.0, (0.0, 0.0), VP, (400, 300), (200, 150));
        match (full, half) {
            (DisplayMath::Fit { scale_factor: a }, DisplayMath::Fit { scale_factor: b }) => {
                // shown_w differs (800 both ways here since both contain to
                // viewport width). Equality of on-screen size is what counts.
                assert!((a - b).abs() < 1e-5, "expected {a} == {b}");
            }
            other => panic!("expected Fit paths, got {other:?}"),
        }
    }

    #[test]
    fn overflowing_zoom_crops_centered_window() {
        // 2000×1000 at 100% in 800×600: window = 800×600 source pixels,
        // centered → x = 600, y = 200.
        let math = display_math(1.0, (0.0, 0.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 600,
                    y: 200,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn pan_shifts_crop_window() {
        // Positive pan.x shifts the image right = window moves left.
        let math = display_math(1.0, (100.0, 0.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 500,
                    y: 200,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn crop_clamps_at_image_edges() {
        let math = display_math(1.0, (10_000.0, 10_000.0), VP, (2000, 1000), (2000, 1000));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            }
        );
    }

    #[test]
    fn crop_rect_maps_into_downscaled_texture_space() {
        // Same view as overflowing_zoom_crops_centered_window, but the
        // texture is half resolution: every coordinate halves.
        let math = display_math(1.0, (0.0, 0.0), VP, (2000, 1000), (1000, 500));
        assert_eq!(
            math,
            DisplayMath::Crop {
                rect: Rectangle {
                    x: 300,
                    y: 100,
                    width: 400,
                    height: 300,
                }
            }
        );
    }

    #[test]
    fn degenerate_inputs_are_empty() {
        assert_eq!(
            display_math(0.0, (0.0, 0.0), VP, (100, 100), (100, 100)),
            DisplayMath::Empty
        );
        assert_eq!(
            display_math(1.0, (0.0, 0.0), VP, (0, 100), (0, 100)),
            DisplayMath::Empty
        );
    }
}
