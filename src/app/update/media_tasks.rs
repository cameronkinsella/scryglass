use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use iced::widget::image::Handle;
use iced::{Size, Task};

use crate::app::state::{CachedImage, DisplayedImage, LoadedMedia, Thumb, Viewer};
use crate::app::viewer_math::compute_zoom;
use crate::app::{App, MediaMessage, Message};
use crate::cache;
use crate::config::ZoomMode;
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
    let DisplayedImage::Full { allocation, .. } = &viewer.displayed else {
        return Task::none();
    };
    let delta = (4 + viewer.rotation - viewer.displayed_rotation) % 4;
    let baked = viewer.rotation;
    let handle = allocation.handle().clone();
    let path = viewer.nav.current().to_path_buf();

    Task::perform(
        async move { tokio::task::spawn_blocking(move || rotate_pixels(&handle, delta)).await },
        |r| r.ok().flatten(),
    )
    .then(move |rotated| {
        let Some((width, height, pixels)) = rotated else {
            return Task::none();
        };
        let p = path.clone();
        cache::allocate_handle(Handle::from_rgba(width, height, pixels)).map(move |upload| {
            match upload {
                Ok(allocation) => Message::Media(MediaMessage::ViewRotated {
                    path: p.clone(),
                    baked,
                    image: CachedImage {
                        allocation,
                        original_size: (width, height),
                    },
                }),
                // Upload failures leave the previous texture in place.
                Err(_) => Message::Media(MediaMessage::SpinnerTick),
            }
        })
    })
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
pub(crate) fn fire_exif(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
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
    if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
    }
    viewer.pan = (0.0, 0.0);
    viewer.displayed = DisplayedImage::Full {
        allocation: image.allocation,
        original_size: image.original_size,
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

/// Fire a pipeline load for `path` unless it's already cached or in flight.
/// The resulting RGBA is uploaded to the GPU and lands as `MediaLoaded`.
pub(crate) fn fire_load(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    path: PathBuf,
    lane: Lane,
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

    let generation = pipeline.generation();
    let load = pipeline.load(
        viewer.source.clone(),
        path.clone(),
        DecodeOpts::default(),
        lane,
        generation,
    );

    Task::perform(load, |r| r).then(move |result| match result {
        Ok(DecodedMedia::Static(img)) => {
            let original_size = img.original_size;
            let thumb = img.thumbnail.map(|t| Thumb {
                handle: Handle::from_rgba(t.width, t.height, t.pixels),
                size: (t.width, t.height),
                original_size: t.original_size,
            });
            let handle = Handle::from_rgba(img.width, img.height, img.pixels);
            let p = path.clone();
            cache::allocate_handle(handle).map(move |upload| {
                let result = upload
                    .map(|allocation| LoadedMedia::Static {
                        image: CachedImage {
                            allocation,
                            original_size,
                        },
                        thumb: thumb.clone(),
                    })
                    .map_err(|e| MediaError::Decode(format!("gpu upload failed: {e:?}")));
                Message::Media(MediaMessage::Loaded {
                    path: p.clone(),
                    result,
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

/// Warm the prefetch window around the cursor.
pub(crate) fn fire_prefetch(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    depth: usize,
) -> Vec<Task<Message>> {
    viewer
        .nav
        .peek_around(depth)
        .into_iter()
        .map(|p| fire_load(pipeline, viewer, p, Lane::Prefetch))
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

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:04}.png")).collect()
    }

    fn at_scroll(cursor: usize, scroll_x: f32) -> crate::app::App {
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
