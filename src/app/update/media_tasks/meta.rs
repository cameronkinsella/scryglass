//! Metadata tasks for the current image: rotation, EXIF, and file size.

use std::path::PathBuf;

use iced::Task;
use iced::widget::image::Handle;

use crate::app::state::{DisplayedImage, Viewer};
use crate::app::{MediaMessage, Message, Shared, Window};
use crate::media::pipeline::Source;
use crate::media::store::{ImageKey, Store};

/// Rotate the displayed image to the desired view rotation, off-thread. Rotating
/// the pixels (not the geometry) leaves zoom, pan, and crop math unchanged. The
/// override is always re-derived from the store's unrotated original by the total
/// turns, so it never iterates on already-rotated pixels and the store keeps one
/// shared original.
pub(crate) fn fire_rotate(viewer: &mut Viewer, store: &Store) -> Task<Message> {
    if viewer.rotation == viewer.displayed_rotation
        || !matches!(viewer.displayed, DisplayedImage::Full { .. })
    {
        return Task::none();
    }
    let path = viewer.nav.current().to_path_buf();
    // A no-op if the source was evicted: a reload restores it, then this fires
    // again as the rotation still differs from what is baked.
    let Some(ram) = store.ram(&ImageKey::new(&viewer.source, &path)) else {
        return Task::none();
    };
    let turns = viewer.rotation;
    let baked = viewer.rotation;
    let source = ram.handle;

    Task::perform(
        async move { tokio::task::spawn_blocking(move || rotate_pixels(&source, turns)).await },
        |r| r.ok().flatten(),
    )
    .then(move |rotated| {
        let Some((width, height, pixels)) = rotated else {
            return Task::none();
        };
        let handle = Handle::from_rgba(width, height, pixels);
        let p = path.clone();
        Task::future(async move {
            let keepalive = super::submit_and_wait(handle).await;
            Message::Media(MediaMessage::ViewRotated {
                path: p.clone(),
                baked,
                original_size: (width, height),
                texture: keepalive,
            })
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
