use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use iced::widget::image::Handle;
use iced::{Size, Task};

use crate::app::state::{CachedImage, DisplayedImage, LoadedMedia, Thumb, Viewer};
use crate::app::viewer_math::compute_zoom;
use crate::app::{MediaMessage, Message, Shared, Window};
use crate::config::{PrefetchVram, ZoomMode};
use crate::media::pipeline::{Lane, Pipeline, Source, ThumbUrgency};
use crate::media::registry::DecodeOpts;
use crate::media::{DecodedMedia, MediaError, ThumbData};

/// Rotate the displayed texture to the desired view rotation, off-thread.
/// Rotating the pixels (not the geometry) leaves zoom, pan, and crop math
/// unchanged. The cache keeps the unrotated original.
pub(crate) fn fire_rotate(viewer: &mut Viewer) -> Task<Message> {
    if viewer.rotation == viewer.displayed_rotation {
        return Task::none();
    }
    let DisplayedImage::Full { handle, .. } = &viewer.displayed else {
        return Task::none();
    };
    let delta = (4 + viewer.rotation - viewer.displayed_rotation) % 4;
    let baked = viewer.rotation;
    let handle = handle.clone();
    let path = viewer.nav.current().to_path_buf();

    Task::perform(
        async move { tokio::task::spawn_blocking(move || rotate_pixels(&handle, delta)).await },
        |r| r.ok().flatten(),
    )
    .then(move |rotated| {
        let Some((width, height, pixels)) = rotated else {
            return Task::none();
        };
        let handle = Handle::from_rgba(width, height, pixels);
        let p = path.clone();
        Task::future(async move {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let keepalive = if crate::ui::image_surface::submit_upload(handle.clone(), ready_tx) {
                ready_rx.await.ok()
            } else {
                None
            };
            Message::Media(MediaMessage::ViewRotated {
                path: p.clone(),
                baked,
                image: CachedImage {
                    handle,
                    original_size: (width, height),
                    keepalive,
                    // Rotation re-uploads the displayed image at full resolution.
                    gpu_full: true,
                    // The rotated image only ever displays; the cache keeps the
                    // unrotated original, so this decode time is never read.
                    decode_time: None,
                },
            })
        })
    })
}

/// Extra resolution kept over the logical viewport so the view-res texture
/// stays crisp at fit on a HiDPI display (whose physical pixels exceed logical).
const VIEW_HEADROOM: f32 = 1.5;

/// Caps how many prefetch downscales run at once, so rapid navigation through
/// fresh neighbors cannot saturate the CPU with resizes.
static RESIZE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// The view-resolution target for an image of `original` size shown at `zoom`
/// (1.0 = full native): the displayed pixel size with HiDPI headroom, never
/// upscaled past native. A demoted but visible image targets its current zoom so
/// it stays as crisp as what is on screen; a prefetch neighbor targets its fit
/// zoom, since it is shown fit when navigated to.
pub(crate) fn view_target(original: (u32, u32), zoom: f32) -> (u32, u32) {
    let (w, h) = (original.0.max(1) as f32, original.1.max(1) as f32);
    let scale = (zoom * VIEW_HEADROOM).clamp(0.0, 1.0);
    (
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    )
}

/// The fit zoom for an image of `original` size in `view`: the uniform scale that
/// makes it fit on both axes, never upscaled. This is what an image shows at when
/// navigated to fresh, so prefetch and restored textures target it.
fn fit_zoom(original: (u32, u32), view: Size) -> f32 {
    let (w, h) = (original.0.max(1) as f32, original.1.max(1) as f32);
    (view.width / w).min(view.height / h).min(1.0)
}

/// Downscale a full-res RGBA handle to `target`, for a prefetch neighbor's
/// smaller GPU texture. Returns the original handle when it already fits. Borrows
/// the source pixels (no full-res copy) so the resize stays cheap.
fn downscale(handle: &Handle, target: (u32, u32)) -> Handle {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return handle.clone();
    };
    if target.0 >= *width || target.1 >= *height {
        return handle.clone();
    }
    let Some(view) =
        image::ImageBuffer::<image::Rgba<u8>, &[u8]>::from_raw(*width, *height, pixels.as_ref())
    else {
        return handle.clone();
    };
    let resized = image::imageops::resize(
        &view,
        target.0,
        target.1,
        image::imageops::FilterType::Triangle,
    );
    Handle::from_rgba(target.0, target.1, resized.into_raw())
}

/// Rotate RGBA pixels behind a handle by quarter turns clockwise.
fn rotate_pixels(handle: &Handle, turns: u8) -> Option<(u32, u32, Vec<u8>)> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return None;
    };
    let buffer = image::RgbaImage::from_raw(*width, *height, pixels.to_vec())?;
    let img = image::DynamicImage::ImageRgba8(buffer);
    let rotated = match turns % 4 {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    };
    let out = rotated.into_rgba8();
    let (w, h) = out.dimensions();
    Some((w, h, out.into_raw()))
}

/// Fetch EXIF fields for the current image (info panel).
pub(crate) fn fire_exif(win: &mut Window, _shared: &mut Shared) -> Task<Message> {
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    let path = viewer.nav.current().to_path_buf();
    // Reuse data already loaded for this file, clear it otherwise.
    if viewer.exif.as_ref().is_some_and(|(p, _)| *p == path) {
        return Task::none();
    }
    viewer.exif = None;
    let load = crate::media::pipeline::load_info(viewer.source.clone(), path.clone());
    Task::perform(load, move |fields| {
        Message::Media(MediaMessage::ExifLoaded(path.clone(), fields))
    })
}

/// Where background thumbnailing should aim, as a `(center, range)` pair to
/// fan outward from: the cursor across the whole directory, or the visible row
/// alone once the cursor has scrolled off the filmstrip.
pub(crate) fn thumb_focus(
    viewer: &Viewer,
    viewport_w: f32,
    filmstrip_shown: bool,
) -> (usize, std::ops::Range<usize>) {
    let len = viewer.nav.len();
    let cursor = viewer.nav.cursor();
    if !filmstrip_shown
        || crate::components::filmstrip::cursor_on_screen(
            viewer.filmstrip_scroll_x,
            cursor,
            viewport_w,
        )
    {
        (cursor, 0..len)
    } else {
        let range =
            crate::components::filmstrip::visible_range(viewer.filmstrip_scroll_x, viewport_w, len);
        let center = range.start + (range.end - range.start) / 2;
        (center, range)
    }
}

/// Put a loaded image on screen, computing zoom from its true dimensions.
pub(crate) fn show_loaded(
    viewer: &mut Viewer,
    path: &std::path::Path,
    image: CachedImage,
    zoom_mode: ZoomMode,
    viewport: Size,
) {
    let (w, h) = image.original_size;
    // Keep the live zoom and pan when a same-path placeholder swaps to full: a
    // pending navigation already set the fit zoom (identical geometry), and a
    // decay restore must preserve the zoom it was viewed at. Only a fresh display
    // recomputes.
    let resuming = matches!(viewer.displayed, DisplayedImage::Placeholder(_))
        && viewer.displayed_path.as_deref() == Some(path);
    if !resuming {
        if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
            viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
        }
        viewer.pan = (0.0, 0.0);
    }
    viewer.displayed = DisplayedImage::Full {
        handle: image.handle,
        original_size: image.original_size,
        texture: image.keepalive,
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
    path: &std::path::Path,
    zoom_mode: ZoomMode,
    viewport: Size,
) -> bool {
    if let Some(thumb) = viewer.thumbs.get(path).cloned() {
        show_placeholder(viewer, path, thumb, zoom_mode, viewport);
        true
    } else {
        viewer.displayed = DisplayedImage::None;
        viewer.displayed_path = None;
        false
    }
}

/// Fire a thumbnail job for `path` unless one is cached, in flight, or
/// known to fail.
pub(crate) fn fire_thumb(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    path: PathBuf,
    urgency: ThumbUrgency,
) -> Task<Message> {
    if viewer.thumbs.contains(&path)
        || viewer.in_flight_thumbs.contains(&path)
        || viewer.failed_thumbs.contains(&path)
    {
        return Task::none();
    }

    let is_video = crate::video::is_video(&path);
    // A video thumbnail is an FFmpeg first-frame grab, which needs a real file
    // on disk, so videos inside archives get none.
    if is_video && !matches!(viewer.source, Source::Fs) {
        return Task::none();
    }

    viewer.in_flight_thumbs.insert(path.clone());
    let generation = pipeline.thumb_generation();
    let load: Pin<Box<dyn Future<Output = Result<ThumbData, MediaError>> + Send>> = if is_video {
        Box::pin(pipeline.load_video_thumb(path.clone(), urgency, generation))
    } else {
        Box::pin(pipeline.load_thumb(viewer.source.clone(), path.clone(), urgency, generation))
    };
    Task::perform(load, move |result| {
        Message::Media(MediaMessage::ThumbLoaded {
            path: path.clone(),
            urgency,
            result: result.map(|data| Thumb {
                handle: Handle::from_rgba(data.width, data.height, data.pixels),
                size: (data.width, data.height),
                original_size: data.original_size,
            }),
        })
    })
}

/// Start (or continue) background thumbnailing: up to `chains` jobs from the
/// current [`thumb_focus`].
pub(crate) fn fire_thumbnailer(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    chains: usize,
    viewport_w: f32,
    filmstrip_shown: bool,
) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for _ in 0..chains {
        let (center, range) = thumb_focus(viewer, viewport_w, filmstrip_shown);
        let Some(path) = viewer.next_unthumbed_in(center, range) else {
            break;
        };
        tasks.push(fire_thumb(pipeline, viewer, path, ThumbUrgency::Background));
    }
    tasks
}

/// How a freshly loaded image's GPU texture is uploaded.
enum Upload {
    /// Decode to RAM only, upload nothing (uploaded on navigation instead).
    Skip,
    /// Downscaled to the fit view resolution (smaller VRAM).
    View,
    /// Full resolution (instant crisp zoom).
    Full,
}

/// Resolve the upload for a load: the current image is always full-res; a
/// prefetch neighbor follows the `prefetch_vram` setting.
fn upload_for(lane: Lane, prefetch_vram: PrefetchVram) -> Upload {
    match (lane, prefetch_vram) {
        (Lane::Current, _) | (Lane::Prefetch, PrefetchVram::FullRes) => Upload::Full,
        (Lane::Prefetch, PrefetchVram::ViewRes) => Upload::View,
        (Lane::Prefetch, PrefetchVram::None) => Upload::Skip,
    }
}

/// Fire a pipeline load for `path` unless it's already cached or in flight.
/// The resulting RGBA is uploaded to the GPU and lands as `MediaLoaded`.
pub(crate) fn fire_load(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    path: PathBuf,
    lane: Lane,
    view: Size,
    prefetch_vram: PrefetchVram,
) -> Task<Message> {
    if viewer.cache.contains(&path)
        || viewer.anim_player.has_cached(&path)
        || viewer.in_flight.contains(&path)
        // Videos never go through the image pipeline. Reading a multi-GB
        // file into memory to fail decoding would be a disaster.
        || crate::video::is_video(&path)
    {
        return Task::none();
    }
    viewer.in_flight.insert(path.clone());

    // Reuse another window's resident decode of this file, sharing its one GPU
    // texture instead of decoding and uploading again.
    if matches!(viewer.source, Source::Fs)
        && let Some((handle, original_size, gpu_full, keepalive)) = pipeline.dedup_get(&path)
    {
        return Task::done(Message::Media(MediaMessage::Loaded {
            path: path.clone(),
            result: Ok(LoadedMedia::Static {
                image: CachedImage {
                    handle,
                    original_size,
                    keepalive: Some(keepalive),
                    // Reuse the shared texture at its real resolution. Reporting
                    // it as not-full would make the display path re-upload, which
                    // forks the shared texture and leaves the other window drawing
                    // a copy it no longer keeps alive.
                    gpu_full,
                    // Reused, not decoded here, so the cost is unknown: never
                    // evict this on a guess.
                    decode_time: None,
                },
                thumb: None,
            }),
        }));
    }

    let generation = pipeline.generation();
    let load = pipeline.load(
        viewer.source.clone(),
        path.clone(),
        DecodeOpts::default(),
        lane,
        generation,
    );

    // Time the read + decode so dynamic eviction can weigh this source's
    // reproduction cost against the memory it holds.
    let timed = async move {
        let start = std::time::Instant::now();
        let result = load.await;
        (result, start.elapsed())
    };
    Task::perform(timed, |x| x).then(move |(result, decode_time)| match result {
        Ok(DecodedMedia::Static(img)) => {
            let original_size = img.original_size;
            let thumb = img.thumbnail.map(|t| Thumb {
                handle: Handle::from_rgba(t.width, t.height, t.pixels),
                size: (t.width, t.height),
                original_size: t.original_size,
            });
            let handle = Handle::from_rgba(img.width, img.height, img.pixels);
            let p = path.clone();
            // TODO: add a config knob to drop the RAM copy after a full-res VRAM
            // upload, holding only the GPU copy.
            let upload = upload_for(lane, prefetch_vram);
            // Hand the texture to the upload thread, then wait for it to become
            // resident before displaying, so navigation never stalls the UI
            // thread on a texture upload. A skipped upload keeps the RAM source
            // only; the display path uploads it from RAM on navigation.
            Task::future(async move {
                let (keepalive, gpu_full) = match upload {
                    Upload::Skip => (None, false),
                    Upload::View => {
                        let zoom = fit_zoom(original_size, view);
                        (
                            upload_at_res(&handle, original_size, zoom, false).await,
                            false,
                        )
                    }
                    Upload::Full => (upload_at_res(&handle, original_size, 1.0, true).await, true),
                };
                Message::Media(MediaMessage::Loaded {
                    path: p.clone(),
                    result: Ok(LoadedMedia::Static {
                        image: CachedImage {
                            handle,
                            original_size,
                            keepalive,
                            gpu_full,
                            decode_time: Some(decode_time),
                        },
                        thumb,
                    }),
                })
            })
        }
        Ok(DecodedMedia::Animated(anim)) => {
            // Frames allocate at display time, only the thumb needs a
            // handle here.
            let thumb = anim.thumbnail.as_ref().map(|t| Thumb {
                handle: Handle::from_rgba(t.width, t.height, t.pixels.clone()),
                size: (t.width, t.height),
                original_size: t.original_size,
            });
            Task::done(Message::Media(MediaMessage::Loaded {
                path: path.clone(),
                result: Ok(LoadedMedia::Animated { anim, thumb }),
            }))
        }
        Err(e) => Task::done(Message::Media(MediaMessage::Loaded {
            path: path.clone(),
            result: Err(e),
        })),
    })
}

/// Re-upload a restored window's resident images from their RAM sources after a
/// minimize freed the GPU textures, displayed image first. Each upload reuses a
/// still-resident shared texture (another window kept it) or re-uploads from the
/// surviving handle, never a disk read, at the same resolution it had.
pub(crate) fn fire_restore_textures(
    pipeline: &Pipeline,
    viewer: &Viewer,
    view: Size,
) -> Vec<Task<Message>> {
    viewer
        .restore_list()
        .into_iter()
        .map(|(path, handle, original_size, gpu_full, decode_time)| {
            let pipeline = pipeline.clone();
            Task::future(async move {
                let keepalive = match pipeline.dedup_get(&path) {
                    Some((_, _, _, keep)) => Some(keep),
                    None => {
                        // Restore each texture at the fit zoom it is shown at when
                        // navigated to; full-res entries ignore the zoom.
                        let zoom = fit_zoom(original_size, view);
                        upload_at_res(&handle, original_size, zoom, gpu_full).await
                    }
                };
                Message::Media(MediaMessage::Reuploaded {
                    path,
                    image: CachedImage {
                        handle,
                        original_size,
                        keepalive,
                        gpu_full,
                        decode_time,
                    },
                })
            })
        })
        .collect()
}

/// Re-upload `path`'s texture at full resolution (`full`, for crisp zoom on the
/// focused image) or downscaled to the view resolution for `zoom` (for an
/// unfocused or passed-by image), from its in-RAM full-res handle (no disk). A
/// no-op when it is already at that resolution or not cached.
pub(crate) fn fire_reupload_res(
    viewer: &Viewer,
    path: &std::path::Path,
    zoom: f32,
    full: bool,
) -> Task<Message> {
    let Some(cached) = viewer.cache.peek(path) else {
        return Task::none();
    };
    // Already at the wanted resolution and still resident: nothing to do. A
    // released texture (keepalive cleared on minimize) re-uploads even when the
    // resolution matches, so a restore re-seats it.
    if cached.gpu_full == full && cached.keepalive.is_some() {
        return Task::none();
    }
    let handle = cached.handle.clone();
    let original_size = cached.original_size;
    let decode_time = cached.decode_time;
    let p = path.to_path_buf();
    Task::future(async move {
        let keepalive = upload_at_res(&handle, original_size, zoom, full).await;
        Message::Media(MediaMessage::Reuploaded {
            path: p,
            image: CachedImage {
                handle,
                original_size,
                keepalive,
                gpu_full: full,
                decode_time,
            },
        })
    })
}

/// Upload `handle` at full resolution (`full`) or downscaled to the view
/// resolution for `zoom`, resolving to the keepalive once resident. Keyed by the
/// handle's own id.
async fn upload_at_res(
    handle: &Handle,
    original_size: (u32, u32),
    zoom: f32,
    full: bool,
) -> Option<crate::ui::image_surface::Keepalive> {
    let gpu_handle = if full {
        handle.clone()
    } else {
        // Bound concurrent downscales so spamming through new neighbors never
        // saturates the CPU with resizes.
        let _permit = RESIZE_GATE.acquire().await.ok();
        let h = handle.clone();
        tokio::task::spawn_blocking(move || downscale(&h, view_target(original_size, zoom)))
            .await
            .unwrap_or_else(|_| handle.clone())
    };
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    if crate::ui::image_surface::submit_upload_at(handle.id(), gpu_handle, ready_tx) {
        ready_rx.await.ok()
    } else {
        None
    }
}

/// Warm the prefetch window around the cursor, each neighbor uploaded per the
/// `prefetch_vram` setting (view-res, full-res, or RAM only).
pub(crate) fn fire_prefetch(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    depth: usize,
    view: Size,
    prefetch_vram: PrefetchVram,
) -> Vec<Task<Message>> {
    viewer
        .nav
        .peek_around(depth)
        .into_iter()
        .map(|p| fire_load(pipeline, viewer, p, Lane::Prefetch, view, prefetch_vram))
        .collect()
}

/// Resolve the current image's byte size: instantly from the archive
/// index, or via an async stat for filesystem images.
pub(crate) fn probe_size(viewer: &mut Viewer, path: PathBuf) -> Task<Message> {
    match &viewer.source {
        Source::Fs => probe_file_size(path),
        Source::Archive(index) => {
            viewer.current_file_size = index.entry_size(&path);
            Task::none()
        }
    }
}

/// Fetch the file size off-thread. A stat on slow storage can stall, and
/// must never run inside `update()`.
fn probe_file_size(path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let size = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            (path, size)
        },
        |(path, size)| Message::Media(MediaMessage::FileSizeProbed(path, size)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn view_target_downscales_to_the_fit_zoom_with_headroom() {
        // 4000x3000 fit into 800x600 is a 0.2 zoom; x1.5 headroom -> 0.3 -> 1200x900.
        let zoom = fit_zoom((4000, 3000), Size::new(800.0, 600.0));
        assert_eq!(view_target((4000, 3000), zoom), (1200, 900));
    }

    #[test]
    fn view_target_never_upscales_past_native() {
        // A small image at fit zoom, and even zoomed way in, stays at native.
        let zoom = fit_zoom((100, 80), Size::new(800.0, 600.0));
        assert_eq!(view_target((100, 80), zoom), (100, 80));
        assert_eq!(view_target((100, 80), 8.0), (100, 80));
    }

    #[test]
    fn view_target_preserves_aspect_on_a_wide_image() {
        // 4000x1000 in 800x600: width binds at 0.2 zoom -> 1200x300.
        let zoom = fit_zoom((4000, 1000), Size::new(800.0, 600.0));
        assert_eq!(view_target((4000, 1000), zoom), (1200, 300));
    }

    #[test]
    fn upload_for_resolves_the_prefetch_vram_mode() {
        // The current image is always full-res, whatever the prefetch setting.
        assert!(matches!(
            upload_for(Lane::Current, PrefetchVram::None),
            Upload::Full
        ));
        // A prefetch neighbor follows the setting.
        assert!(matches!(
            upload_for(Lane::Prefetch, PrefetchVram::FullRes),
            Upload::Full
        ));
        assert!(matches!(
            upload_for(Lane::Prefetch, PrefetchVram::ViewRes),
            Upload::View
        ));
        assert!(matches!(
            upload_for(Lane::Prefetch, PrefetchVram::None),
            Upload::Skip
        ));
    }

    #[test]
    fn view_target_keeps_more_resolution_when_zoomed_in() {
        // A zoomed-in but demoted image targets its zoom, not its fit, so it
        // stays crisp: 4000x3000 at 0.5 zoom -> x1.5 -> 0.75 -> 3000x2250.
        assert_eq!(view_target((4000, 3000), 0.5), (3000, 2250));
    }

    #[test]
    fn downscale_shrinks_to_the_target_dimensions() {
        let handle = Handle::from_rgba(8, 6, vec![200u8; 8 * 6 * 4]);
        let Handle::Rgba { width, height, .. } = downscale(&handle, (4, 3)) else {
            panic!("expected rgba");
        };
        assert_eq!((width, height), (4, 3));
    }

    #[test]
    fn downscale_keeps_an_image_that_already_fits() {
        let handle = Handle::from_rgba(4, 4, vec![200u8; 4 * 4 * 4]);
        let Handle::Rgba { width, height, .. } = downscale(&handle, (8, 8)) else {
            panic!("expected rgba");
        };
        assert_eq!((width, height), (4, 4));
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:04}.png")).collect()
    }

    fn at_scroll(cursor: usize, scroll_x: f32) -> crate::app::test_support::TestApp {
        let ns = names(50);
        let refs: Vec<&str> = ns.iter().map(String::as_str).collect();
        let mut app = viewing_app(&refs, cursor);
        app.viewer_mut().unwrap().filmstrip_scroll_x = scroll_x;
        app
    }

    #[test]
    fn thumb_focus_follows_the_cursor_when_on_screen() {
        let app = at_scroll(2, 0.0);
        assert_eq!(thumb_focus(app.viewer().unwrap(), 800.0, true), (2, 0..50));
    }

    #[test]
    fn thumb_focus_switches_to_the_visible_row_off_screen() {
        let app = at_scroll(2, 3000.0);
        let (center, range) = thumb_focus(app.viewer().unwrap(), 800.0, true);
        let expected = crate::components::filmstrip::visible_range(3000.0, 800.0, 50);
        assert_eq!(range, expected);
        assert_eq!(center, expected.start + (expected.end - expected.start) / 2);
        assert_ne!(center, 2);
    }

    #[test]
    fn thumb_focus_ignores_the_scroll_when_the_filmstrip_is_hidden() {
        let app = at_scroll(2, 3000.0);
        assert_eq!(thumb_focus(app.viewer().unwrap(), 800.0, false), (2, 0..50));
    }
}
