//! Async media tasks fired from the update loop: loads and prefetch,
//! tile production, thumbnails, and metadata probes, plus the shared
//! display-state and sizing helpers.

mod load;
mod meta;
mod thumbs;
mod tiles;

pub(crate) use load::{
    fire_load, fire_prefetch, run_jobs, run_jobs_at, set_prefetch_scaler, try_start_shared_anim,
};
pub(crate) use meta::{fire_exif, fire_rotate, probe_size};
pub(crate) use thumbs::{fire_archive_video_thumb, fire_thumb, fire_thumbnailer};
pub(crate) use tiles::{fire_tiles, settle_tiles};

use iced::Size;
use iced::widget::image::Handle;

use crate::app::state::{DisplayedImage, Thumb, Viewer};
use crate::app::viewer_math::{auto_zoom, compute_zoom};
use crate::config::{PrefetchVram, ZoomMode};
use crate::media::cache::ImageCache;
use crate::media::pipeline::{Lane, thumb_key};
use crate::media::store::Tier;

/// The view-resolution target for an image of `original` size shown at `zoom`
/// (1.0 = full native): the displayed size in physical pixels (logical size
/// times `scale_factor`), never upscaled past native. Sizing to the physical
/// display keeps the demoted copy crisp on HiDPI and seamless with full-res.
/// A visible demote targets its current zoom, a prefetch neighbor its fit zoom.
pub(crate) fn view_target(original: (u32, u32), zoom: f32, scale_factor: f32) -> (u32, u32) {
    let (w, h) = (original.0.max(1) as f32, original.1.max(1) as f32);
    // The view copy must always fit one texture. Past this cap the tile
    // pyramid carries the zoom.
    let max = crate::media::registry::MAX_TEXTURE_DIM as f32;
    let scale = (zoom * scale_factor.max(1.0))
        .clamp(0.0, 1.0)
        .min(max / w)
        .min(max / h);
    (
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    )
}

/// The fit zoom for an image of `original` size in `view`. Delegates to
/// [`auto_zoom`], the zoom an image shows at when navigated to fresh, so
/// prefetch and restored textures bake to exactly what the display computes.
fn fit_zoom(original: (u32, u32), view: Size) -> f32 {
    auto_zoom(original.0, original.1, view)
}

/// Put a loaded still on screen, computing zoom from its true dimensions. The
/// pixels are derived from the store at render time (the lease's texture, else
/// the thumbnail blur), so this only needs the true size and the path. While the
/// texture is still uploading, the view shows the blur.
pub(crate) fn show_loaded(
    viewer: &mut Viewer,
    path: &std::path::Path,
    original_size: (u32, u32),
    zoom_mode: ZoomMode,
    viewport: Size,
) {
    let (w, h) = original_size;
    // Keep the live zoom and pan when the same image is already on screen: a pending
    // navigation's placeholder swapping to full (its fit zoom is already set), or a
    // decayed image re-decoding after eviction (still shown Full through its blur,
    // and it must return to the zoom it was viewed at). Only a fresh display, where
    // the path differs or nothing is shown, recomputes.
    let resuming = viewer.displayed_path.as_deref() == Some(path)
        && matches!(
            viewer.displayed,
            DisplayedImage::Placeholder(_) | DisplayedImage::Full { .. }
        );
    if !resuming {
        if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
            viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
        }
        viewer.pan = (0.0, 0.0);
    }
    viewer.displayed = DisplayedImage::Full {
        original_size,
        rotated: None,
    };
    viewer.displayed_path = Some(path.to_path_buf());
    viewer.pending_since = None;
}

/// Show a placeholder thumbnail while the full image loads. Zoom uses the
/// true dimensions, so geometry is identical when the full image swaps in
/// (no jump). The load stays pending.
pub(crate) fn show_placeholder(
    viewer: &mut Viewer,
    path: &std::path::Path,
    thumb: Thumb,
    zoom_mode: ZoomMode,
    viewport: Size,
) {
    let (w, h) = thumb.original_size;
    if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
    }
    viewer.pan = (0.0, 0.0);
    viewer.displayed = DisplayedImage::Placeholder(thumb);
    viewer.displayed_path = Some(path.to_path_buf());
}

/// Show the cached thumbnail for `path` if there is one, otherwise clear
/// the image area. Returns true when a placeholder was shown. Either way
/// the image area now refers to `path`, never to a previous image.
pub(crate) fn show_placeholder_or_clear(
    viewer: &mut Viewer,
    thumbs: &ImageCache<Thumb>,
    path: &std::path::Path,
    zoom_mode: ZoomMode,
    viewport: Size,
) -> bool {
    if let Some(thumb) = thumbs.peek(&thumb_key(&viewer.source, path)).cloned() {
        show_placeholder(viewer, path, thumb, zoom_mode, viewport);
        true
    } else {
        viewer.displayed = DisplayedImage::None;
        viewer.displayed_path = None;
        false
    }
}

/// The store lease tier a prefetched neighbor is held at, from the
/// `prefetch_vram` setting: a full-res texture, a downscaled view-res texture,
/// or RAM only (uploaded on navigation).
pub(crate) fn prefetch_want(prefetch_vram: PrefetchVram) -> Tier {
    match prefetch_vram {
        PrefetchVram::FullRes => Tier::Full,
        PrefetchVram::ViewRes => Tier::View,
        PrefetchVram::None => Tier::InRam,
    }
}

/// The decode priority lane for a wanted tier: the on-screen image (wants Full)
/// is urgent, a prefetch neighbor rides the background lane.
fn lane_for(want: Tier) -> Lane {
    if want >= Tier::Full {
        Lane::Current
    } else {
        Lane::Prefetch
    }
}

/// Submit `handle` to the upload thread and wait for its keepalive.
async fn submit_and_wait(handle: Handle) -> Option<crate::ui::image_surface::Keepalive> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    if crate::ui::image_surface::submit_upload(handle, ready_tx) {
        ready_rx.await.ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_target_sizes_to_the_physical_display() {
        // 4000x3000 fit into 800x600 is a 0.2 zoom. At 100% scaling the copy is the
        // logical size. A 200% display doubles it to stay crisp on the denser panel.
        let zoom = fit_zoom((4000, 3000), Size::new(800.0, 600.0));
        assert_eq!(view_target((4000, 3000), zoom, 1.0), (800, 600));
        assert_eq!(view_target((4000, 3000), zoom, 2.0), (1600, 1200));
    }

    #[test]
    fn view_target_never_upscales_past_native() {
        // A small image at fit zoom, and even zoomed way in on a dense display,
        // stays at native.
        let zoom = fit_zoom((100, 80), Size::new(800.0, 600.0));
        assert_eq!(view_target((100, 80), zoom, 1.0), (100, 80));
        assert_eq!(view_target((100, 80), 8.0, 2.0), (100, 80));
    }

    #[test]
    fn view_target_preserves_aspect_on_a_wide_image() {
        // 4000x1000 in 800x600: width binds at 0.2 zoom -> 800x200 at 100% scaling.
        let zoom = fit_zoom((4000, 1000), Size::new(800.0, 600.0));
        assert_eq!(view_target((4000, 1000), zoom, 1.0), (800, 200));
    }

    #[test]
    fn view_target_never_exceeds_one_texture() {
        // A tiled-regime (uncapped) source at high zoom caps the view copy to
        // the texture limit, aspect preserved. Tiles carry the rest.
        let max = crate::media::registry::MAX_TEXTURE_DIM;
        assert_eq!(view_target((20000, 10000), 1.0, 1.0), (max, max / 2));
        assert_eq!(view_target((20000, 10000), 0.1, 1.0), (2000, 1000));
    }

    #[test]
    fn prefetch_want_resolves_the_prefetch_vram_mode() {
        // The prefetch tier follows the setting. The current image always asks
        // for Full at its call site.
        assert_eq!(prefetch_want(PrefetchVram::FullRes), Tier::Full);
        assert_eq!(prefetch_want(PrefetchVram::ViewRes), Tier::View);
        assert_eq!(prefetch_want(PrefetchVram::None), Tier::InRam);
    }

    #[test]
    fn view_target_keeps_more_resolution_when_zoomed_in() {
        // A zoomed-in but demoted image targets its zoom, not its fit, so it stays
        // crisp: 4000x3000 at 0.5 zoom on a 100% display -> 2000x1500.
        assert_eq!(view_target((4000, 3000), 0.5, 1.0), (2000, 1500));
    }
}
