use std::path::PathBuf;

use crate::media::MediaError;
use crate::media::pipeline::ThumbUrgency;

use crate::app::state::{CachedImage, LoadedMedia, Session, Thumb};

#[derive(Debug, Clone)]
pub enum Message {
    Loaded {
        path: PathBuf,
        result: Result<LoadedMedia, MediaError>,
    },
    ThumbLoaded {
        path: PathBuf,
        urgency: ThumbUrgency,
        result: Result<Thumb, MediaError>,
    },
    FileSizeProbed(PathBuf, u64),
    ExifLoaded(PathBuf, Vec<(String, String)>),
    ViewRotated {
        path: PathBuf,
        baked: u8,
        image: CachedImage,
    },
    /// A minimized window's image came back from its RAM source after restore.
    /// Only re-seats the texture (cache keepalive + cross-window dedup); the
    /// displayed image is untouched so pan and zoom survive the round trip.
    Reuploaded {
        path: PathBuf,
        image: CachedImage,
    },
    /// Debounced after navigation settles on a view-res neighbor: promote it to
    /// a full-res texture so zoom is crisp, unless navigation has moved on.
    PromoteCurrent(PathBuf),
    Resorted(Vec<PathBuf>),
    SpinnerTick,
}
use iced::Task;

use crate::anim::AnimMessage;
use crate::app::state::DisplayedImage;
use crate::app::update::{
    complete_navigation, fire_load, fire_rotate, fire_thumbnailer, resolve_pending_nav,
    show_loaded, show_placeholder,
};
use crate::app::viewer_math::compute_zoom;
use crate::app::{Message as AppMessage, Shared, Window};
use crate::components::filmstrip;
use crate::config::ZoomMode;
use crate::media::pipeline::Lane;

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Loaded { path, result } => {
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            let depth = shared.config.prefetch_depth;
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };

            viewer.in_flight.remove(&path);

            match result {
                Ok(LoadedMedia::Static { image, thumb }) => {
                    viewer
                        .cache
                        .insert(path.clone(), image.clone(), image.byte_cost());
                    // Register for cross-window reuse while its texture stays
                    // resident, so another window opening this file shares it.
                    if let Some(keepalive) = &image.keepalive {
                        pipeline.dedup_insert(
                            path.clone(),
                            image.handle.clone(),
                            image.original_size,
                            keepalive,
                        );
                    }
                    if let Some(thumb) = thumb {
                        let cost = thumb.byte_cost();
                        viewer.thumbs.insert(path.clone(), thumb, cost);
                    }
                    if viewer.nav.current() == path {
                        show_loaded(viewer, &path, image, zoom_mode, viewport);
                    }
                    let pinned = viewer.pinned_paths(depth);
                    viewer.cache.evict_over_budget(&pinned);
                    viewer.thumbs.evict_over_budget(&pinned);
                    resolve_pending_nav(win, shared)
                }
                Ok(LoadedMedia::Animated { anim, thumb }) => {
                    if let Some(thumb) = thumb {
                        let cost = thumb.byte_cost();
                        viewer.thumbs.insert(path.clone(), thumb, cost);
                    }
                    viewer.anim_player.insert(path.clone(), anim);
                    let play = if viewer.nav.current() == path {
                        viewer
                            .anim_player
                            .try_start_from_cache(&path)
                            .map(|t| t.map(AppMessage::Anim))
                            .unwrap_or_else(Task::none)
                    } else {
                        Task::none()
                    };
                    Task::batch([play, resolve_pending_nav(win, shared)])
                }
                Err(MediaError::Cancelled) => {
                    let pending_path = viewer
                        .pending_nav
                        .map(|i| viewer.nav.files()[i].to_path_buf());
                    if viewer.nav.current() == path || pending_path.as_deref() == Some(&*path) {
                        fire_load(&pipeline, viewer, path, Lane::Current, viewport)
                    } else if viewer.pinned_paths(depth).contains(&path) {
                        fire_load(&pipeline, viewer, path, Lane::Prefetch, viewport)
                    } else {
                        Task::none()
                    }
                }
                Err(err) => {
                    let pending_index = viewer.pending_nav;
                    let pending_path = pending_index.map(|i| viewer.nav.files()[i].to_path_buf());
                    let is_current = viewer.nav.current() == path;
                    let is_pending = pending_path.as_deref() == Some(&*path);
                    if !is_current && !is_pending {
                        return Task::none();
                    }
                    viewer.pending_since = None;
                    if is_pending {
                        viewer.pending_nav = None;
                    }
                    // The file vanished (deleted outside the app): drop it and
                    // move on instead of erroring. The watcher usually removes it
                    // first. This is the backstop for the race.
                    if !path.exists() {
                        viewer.cache.remove(&path);
                        viewer.thumbs.remove(&path);
                        viewer.anim_player.remove(&path);
                        viewer.failed_thumbs.remove(&path);
                        viewer.failed_loads.remove(&path);
                        if !viewer.nav.remove(&path) {
                            win.session = Session::Empty;
                            return Task::none();
                        }
                        let cursor = viewer.nav.cursor();
                        return complete_navigation(win, shared, cursor, true);
                    }
                    // The file exists but won't decode (a video renamed to .png,
                    // a truncated image). Remember it and show the error in place.
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let message = format!("{name}\n\n{err}");
                    viewer.failed_loads.insert(path.clone(), message.clone());

                    if is_pending && let Some(index) = pending_index {
                        return complete_navigation(win, shared, index, false);
                    }
                    // The current file failed in place: show the error unless a
                    // good image for it is already on screen.
                    let already_shown = matches!(viewer.displayed, DisplayedImage::Full { .. })
                        && viewer.displayed_path.as_deref() == Some(&*path);
                    if !already_shown {
                        viewer.displayed = DisplayedImage::Error { message };
                        viewer.displayed_path = Some(path.clone());
                    }
                    Task::none()
                }
            }
        }

        Message::ThumbLoaded {
            path,
            urgency,
            result,
        } => {
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            let window_w = win.window_size.width;
            let show_filmstrip = shared.config.show_filmstrip;
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };

            match result {
                Ok(thumb) => {
                    viewer.in_flight_thumbs.remove(&path);
                    let cost = thumb.byte_cost();
                    viewer.thumbs.insert(path.clone(), thumb.clone(), cost);
                    if viewer.nav.current() == path
                        && viewer.pending_since.is_some()
                        && viewer.pending_nav.is_none()
                    {
                        show_placeholder(viewer, &path, thumb, zoom_mode, viewport);
                    }
                }
                // A jump cleared this slot. A re-fire may own it now, so leave
                // in_flight alone. The pump re-picks the path if nothing did.
                Err(MediaError::Cancelled) => {}
                Err(_) => {
                    viewer.in_flight_thumbs.remove(&path);
                    if urgency == ThumbUrgency::Background {
                        viewer.failed_thumbs.insert(path.clone());
                    }
                }
            }

            let mut tasks = fire_thumbnailer(&pipeline, viewer, 1, window_w, show_filmstrip);
            tasks.push(resolve_pending_nav(win, shared));
            Task::batch(tasks)
        }

        Message::FileSizeProbed(path, size) => {
            if let Some(viewer) = win.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.current_file_size = Some(size);
            }
            Task::none()
        }

        Message::SpinnerTick => Task::none(),

        Message::Resorted(files) => {
            let window_id = win.id;
            let window_w = win.window_size.width;
            let show_filmstrip = shared.config.show_filmstrip;
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.nav.replace_files(files);

            if viewer.resort_to_first {
                viewer.resort_to_first = false;
                if viewer.nav.cursor() != 0 {
                    return complete_navigation(win, shared, 0, true);
                }
            }

            let mut tasks = Vec::new();
            if show_filmstrip {
                // A resort reshuffles the whole strip, so recenter the cursor
                // like a fresh open.
                let offset =
                    filmstrip::open_offset(viewer.nav.cursor(), window_w, viewer.nav.len());
                viewer.filmstrip_scroll_x = offset;
                tasks.push(iced::widget::operation::scroll_to(
                    filmstrip::filmstrip_id(window_id),
                    iced::widget::scrollable::AbsoluteOffset { x: offset, y: 0.0 },
                ));
                tasks.extend(fire_thumbnailer(
                    &pipeline,
                    viewer,
                    3,
                    window_w,
                    show_filmstrip,
                ));
            }
            Task::batch(tasks)
        }

        Message::ViewRotated { path, baked, image } => {
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            if viewer.nav.current() != path
                || !matches!(viewer.displayed, DisplayedImage::Full { .. })
            {
                return Task::none();
            }

            let (w, h) = image.original_size;
            viewer.displayed = DisplayedImage::Full {
                handle: image.handle,
                original_size: image.original_size,
            };
            viewer.displayed_rotation = baked;
            viewer.pan = (0.0, 0.0);
            if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }

            fire_rotate(viewer)
        }

        Message::Reuploaded { path, image } => {
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            // Only re-seat the texture if the file is still cached (navigation
            // during restore may have evicted it). Refresh dedup so another
            // window reopening it shares this texture again.
            if viewer.cache.contains(&path) {
                if let Some(keepalive) = &image.keepalive {
                    pipeline.dedup_insert(
                        path.clone(),
                        image.handle.clone(),
                        image.original_size,
                        keepalive,
                    );
                }
                let cost = image.byte_cost();
                viewer.cache.insert(path, image, cost);
            }
            Task::none()
        }

        Message::PromoteCurrent(path) => {
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            // Only promote if navigation is still resting on this image, so a
            // quick pass-through never re-uploads it at full resolution.
            if viewer.displayed_path.as_deref() == Some(&*path) {
                let zoom = viewer.zoom;
                crate::app::update::fire_reupload_res(viewer, &path, zoom, true)
            } else {
                Task::none()
            }
        }

        Message::ExifLoaded(path, fields) => {
            if let Some(viewer) = win.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.exif = Some((path, fields));
            }
            Task::none()
        }
    }
}
pub(crate) fn update_anim(
    win: &mut Window,
    shared: &mut Shared,
    anim_msg: AnimMessage,
) -> Task<AppMessage> {
    let zoom_mode = shared.config.zoom_mode;
    let viewport = win.viewport_size;
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };

    let is_first_frame = matches!(viewer.displayed, DisplayedImage::None)
        || (viewer.pending_since.is_some() && matches!(&anim_msg, AnimMessage::FrameAllocated(..)));

    let (task, frame) = viewer.anim_player.update(anim_msg, viewer.nav.current());

    if let Some(handle) = frame {
        let (w, h) = match &handle {
            iced::widget::image::Handle::Rgba { width, height, .. } => (*width, *height),
            _ => (0, 0),
        };
        if is_first_frame && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
            viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            viewer.pan = (0.0, 0.0);
        }
        viewer.displayed = DisplayedImage::Full {
            handle,
            original_size: (w, h),
        };
        viewer.displayed_path = Some(viewer.nav.current().to_path_buf());
        viewer.pending_since = None;
    }

    // A pending move onto a GIF resolves once its decode lands.
    Task::batch([task.map(AppMessage::Anim), resolve_pending_nav(win, shared)])
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::app::test_support::{thumb, viewing_app};

    #[test]
    fn thumb_loaded_caches_the_blur_and_clears_in_flight() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut()
            .unwrap()
            .in_flight_thumbs
            .insert("a.png".into());
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ThumbLoaded {
                path: "a.png".into(),
                urgency: ThumbUrgency::Urgent,
                result: Ok(thumb(4, 4)),
            },
        );
        let v = app.viewer().unwrap();
        assert!(v.thumbs.contains(Path::new("a.png")));
        assert!(!v.in_flight_thumbs.contains(Path::new("a.png")));
    }

    #[test]
    fn a_failed_background_thumb_is_remembered() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ThumbLoaded {
                path: "b.png".into(),
                urgency: ThumbUrgency::Background,
                result: Err(crate::media::MediaError::Unsupported),
            },
        );
        assert!(
            app.viewer()
                .unwrap()
                .failed_thumbs
                .contains(Path::new("b.png"))
        );
    }

    #[test]
    fn a_cancelled_thumb_keeps_its_slot_and_is_not_failed() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut()
            .unwrap()
            .in_flight_thumbs
            .insert("b.png".into());
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ThumbLoaded {
                path: "b.png".into(),
                urgency: ThumbUrgency::Background,
                result: Err(crate::media::MediaError::Cancelled),
            },
        );
        let v = app.viewer().unwrap();
        // A re-fire after the jump may own the slot, so it isn't cleared, and a
        // stale cancellation never marks the file as failed.
        assert!(v.in_flight_thumbs.contains(Path::new("b.png")));
        assert!(!v.failed_thumbs.contains(Path::new("b.png")));
    }

    #[test]
    fn file_size_probe_updates_the_current_file() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::FileSizeProbed("a.png".into(), 4096),
        );
        assert_eq!(app.viewer().unwrap().current_file_size, Some(4096));
    }

    #[test]
    fn a_stale_file_size_probe_is_ignored() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::FileSizeProbed("b.png".into(), 4096),
        );
        assert_eq!(app.viewer().unwrap().current_file_size, None);
    }

    #[test]
    fn resort_replaces_the_file_order() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resorted(vec!["c.png".into(), "b.png".into(), "a.png".into()]),
        );
        assert_eq!(app.viewer().unwrap().nav.files()[0], PathBuf::from("c.png"));
    }

    #[test]
    fn spinner_tick_changes_nothing() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::SpinnerTick);
        assert_eq!(app.viewer().unwrap().nav.cursor(), 0);
    }

    #[test]
    fn reupload_reseats_the_texture_for_a_cached_file() {
        let mut app = viewing_app(&["a.png"], 0);
        let handle = iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]);
        // Simulate a minimized window: the file is cached but its keepalive was
        // released, so the texture is gone while the RAM source survives.
        let released = CachedImage {
            handle: handle.clone(),
            original_size: (2, 2),
            keepalive: None,
            gpu_full: false,
        };
        let cost = released.byte_cost();
        app.viewer_mut()
            .unwrap()
            .cache
            .insert("a.png".into(), released, cost);

        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Reuploaded {
                path: "a.png".into(),
                image: CachedImage {
                    handle,
                    original_size: (2, 2),
                    keepalive: Some(crate::ui::image_surface::test_keepalive()),
                    gpu_full: true,
                },
            },
        );

        let v = app.viewer().unwrap();
        assert!(
            v.cache
                .peek(Path::new("a.png"))
                .unwrap()
                .keepalive
                .is_some()
        );
    }

    #[test]
    fn reupload_ignores_a_file_evicted_during_restore() {
        let mut app = viewing_app(&["a.png"], 0);
        // Nothing cached for the path: a navigation during restore evicted it.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Reuploaded {
                path: "gone.png".into(),
                image: CachedImage {
                    handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]),
                    original_size: (2, 2),
                    keepalive: Some(crate::ui::image_surface::test_keepalive()),
                    gpu_full: true,
                },
            },
        );
        assert!(!app.viewer().unwrap().cache.contains(Path::new("gone.png")));
    }

    #[test]
    fn a_broken_file_becomes_a_navigable_error_stop() {
        use std::io::Write;
        // Real files so the not-found backstop doesn't fire. The cursor starts
        // on `a` with a pending move onto the (undecodable) `b`.
        let dir = std::env::temp_dir().join(format!("scryglass-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        for p in [&a, &b] {
            std::fs::File::create(p)
                .unwrap()
                .write_all(b"not really a png")
                .unwrap();
        }
        let (a_s, b_s) = (
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        );
        let mut app = viewing_app(&[&a_s, &b_s], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.pending_nav = Some(1);
            v.pending_since = Some(iced::time::Instant::now());
        }

        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Loaded {
                path: b.clone(),
                result: Err(crate::media::MediaError::Decode("bad".into())),
            },
        );

        let v = app.viewer().unwrap();
        // The cursor crossed onto the broken file rather than stalling before it,
        // and the file now shows an error instead of nothing.
        assert_eq!(v.nav.cursor(), 1);
        assert!(matches!(v.displayed, DisplayedImage::Error { .. }));
        assert!(v.failed_loads.contains_key(b.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
