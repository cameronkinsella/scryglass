use std::path::PathBuf;

use iced::widget::image::Handle;
use iced::{Size, Task};

use crate::app::state::{CachedImage, DisplayedImage, LoadedMedia, Thumb, Viewer};
use crate::app::viewer_math::compute_zoom;
use crate::app::{App, Message};
use crate::cache;
use crate::config::ZoomMode;
use crate::media::pipeline::{Lane, Pipeline, Source, ThumbUrgency};
use crate::media::registry::DecodeOpts;
use crate::media::{DecodedMedia, MediaError};
use crate::ui;

/// Rotate the displayed texture to match the desired view rotation.
/// Rotation happens on the pixels (off-thread) so every bit of zoom, pan,
/// and crop math keeps working on the rotated dimensions unchanged. The
/// cache keeps the unrotated original.
pub(super) fn fire_rotate(viewer: &mut Viewer) -> Task<Message> {
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
                Ok(allocation) => Message::ViewRotated {
                    path: p.clone(),
                    baked,
                    image: CachedImage {
                        allocation,
                        original_size: (width, height),
                    },
                },
                // Upload failures leave the previous texture in place.
                Err(_) => Message::SpinnerTick,
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
pub(super) fn fire_exif(app: &mut App) -> Task<Message> {
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
        Message::ExifLoaded(path.clone(), fields)
    })
}

/// Fire thumbnail probes for every filmstrip cell currently in view.
pub(super) fn fire_visible_thumbs(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    viewport_w: f32,
) -> Vec<Task<Message>> {
    let range =
        ui::filmstrip::visible_range(viewer.filmstrip_scroll_x, viewport_w, viewer.nav.len());
    let paths: Vec<PathBuf> = viewer.nav.files()[range].to_vec();
    paths
        .into_iter()
        .map(|p| fire_thumb(pipeline, viewer, p, ThumbUrgency::Background))
        .collect()
}

/// Put a loaded image on screen, computing zoom from its true dimensions.
pub(super) fn show_loaded(
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

/// Put a blurred thumbnail on screen while the full image decodes. Zoom is
/// computed from the true dimensions, so geometry is identical when the
/// full image swaps in, no jump. The load stays pending (spinner included).
pub(super) fn show_placeholder(
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
pub(super) fn show_placeholder_or_clear(
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
pub(super) fn fire_thumb(
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

    if crate::video::is_video(&path) {
        // First-frame grab via FFmpeg, which needs a real file on disk.
        if !matches!(viewer.source, Source::Fs) {
            return Task::none();
        }
        viewer.in_flight_thumbs.insert(path.clone());
        let disk = pipeline.disk();
        let probe = async move {
            tokio::task::spawn_blocking(move || {
                let container = path
                    .parent()
                    .unwrap_or(std::path::Path::new(""))
                    .to_path_buf();
                let name = path.file_name().unwrap_or_default().to_owned();
                if let Some(disk) = &disk
                    && let Some(hit) = disk.load(&container, &name)
                {
                    return (path, Ok(hit));
                }
                match crate::video::first_frame_thumb(&path, crate::media::THUMB_DIM) {
                    Some(thumb) => {
                        if let Some(disk) = &disk {
                            disk.store(&container, &name, &thumb, None, 0);
                        }
                        (path, Ok(thumb))
                    }
                    None => (path, Err(MediaError::Unsupported)),
                }
            })
            .await
            .expect("video thumb task panicked")
        };
        return Task::perform(probe, move |(path, result)| Message::ThumbLoaded {
            path,
            urgency,
            result: result.map(|data| Thumb {
                handle: Handle::from_rgba(data.width, data.height, data.pixels),
                size: (data.width, data.height),
                original_size: data.original_size,
            }),
        });
    }

    viewer.in_flight_thumbs.insert(path.clone());
    let load = pipeline.load_thumb(viewer.source.clone(), path.clone(), urgency);
    Task::perform(load, move |result| Message::ThumbLoaded {
        path: path.clone(),
        urgency,
        result: result.map(|data| Thumb {
            handle: Handle::from_rgba(data.width, data.height, data.pixels),
            size: (data.width, data.height),
            original_size: data.original_size,
        }),
    })
}

/// Start (or continue) background thumbnailing: up to `chains` parallel
/// job streams that work outward from the cursor until every file in the
/// directory has a thumbnail.
pub(super) fn fire_thumbnailer(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    chains: usize,
) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for _ in 0..chains {
        let Some(path) = viewer.next_unthumbed() else {
            break;
        };
        tasks.push(fire_thumb(pipeline, viewer, path, ThumbUrgency::Background));
    }
    tasks
}

/// Fire a pipeline load for `path` unless it's already cached or in flight.
/// The resulting RGBA is uploaded to the GPU and lands as `MediaLoaded`.
pub(super) fn fire_load(
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
                Message::MediaLoaded {
                    path: p.clone(),
                    result,
                }
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
            Task::done(Message::MediaLoaded {
                path: path.clone(),
                result: Ok(LoadedMedia::Animated { anim, thumb }),
            })
        }
        Err(e) => Task::done(Message::MediaLoaded {
            path: path.clone(),
            result: Err(e),
        }),
    })
}

/// Warm the prefetch window around the cursor.
pub(super) fn fire_prefetch(
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
pub(super) fn probe_size(viewer: &mut Viewer, path: PathBuf) -> Task<Message> {
    match &viewer.source {
        Source::Fs => probe_file_size(path),
        Source::Archive(index) => {
            viewer.current_file_size = index.entry_size(&path);
            Task::none()
        }
    }
}

/// Fetch the file size off-thread, a stat on slow storage can stall for seconds and
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
        |(path, size)| Message::FileSizeProbed(path, size),
    )
}
