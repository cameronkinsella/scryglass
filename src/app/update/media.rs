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
use crate::app::{App, Message as AppMessage};
use crate::components::filmstrip;
use crate::config::ZoomMode;
use crate::media::pipeline::Lane;

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Loaded { path, result } => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let depth = app.config.prefetch_depth;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.in_flight.remove(&path);

            match result {
                Ok(LoadedMedia::Static { image, thumb }) => {
                    viewer
                        .cache
                        .insert(path.clone(), image.clone(), image.byte_cost());
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
                    resolve_pending_nav(app)
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
                    Task::batch([play, resolve_pending_nav(app)])
                }
                Err(MediaError::Cancelled) => {
                    let pending_path = viewer
                        .pending_nav
                        .map(|i| viewer.nav.files()[i].to_path_buf());
                    if viewer.nav.current() == path || pending_path.as_deref() == Some(&*path) {
                        fire_load(&pipeline, viewer, path, Lane::Current)
                    } else if viewer.pinned_paths(depth).contains(&path) {
                        fire_load(&pipeline, viewer, path, Lane::Prefetch)
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
                            app.session = Session::Empty;
                            return Task::none();
                        }
                        let cursor = viewer.nav.cursor();
                        return complete_navigation(app, cursor, true);
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
                        return complete_navigation(app, index, false);
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
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let window_w = app.window_size.width;
            let show_filmstrip = app.config.show_filmstrip;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
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
            tasks.push(resolve_pending_nav(app));
            Task::batch(tasks)
        }

        Message::FileSizeProbed(path, size) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.current_file_size = Some(size);
            }
            Task::none()
        }

        Message::SpinnerTick => Task::none(),

        Message::Resorted(files) => {
            let window_w = app.window_size.width;
            let show_filmstrip = app.config.show_filmstrip;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.nav.replace_files(files);

            if viewer.resort_to_first {
                viewer.resort_to_first = false;
                if viewer.nav.cursor() != 0 {
                    return complete_navigation(app, 0, true);
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
                    filmstrip::filmstrip_id(),
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
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            if viewer.nav.current() != path
                || !matches!(viewer.displayed, DisplayedImage::Full { .. })
            {
                return Task::none();
            }

            let (w, h) = image.original_size;
            viewer.displayed = DisplayedImage::Full {
                allocation: image.allocation,
                original_size: image.original_size,
            };
            viewer.displayed_rotation = baked;
            viewer.pan = (0.0, 0.0);
            if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }

            fire_rotate(viewer)
        }

        Message::ExifLoaded(path, fields) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.exif = Some((path, fields));
            }
            Task::none()
        }
    }
}
pub(crate) fn update_anim(app: &mut App, anim_msg: AnimMessage) -> Task<AppMessage> {
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };

    let is_first_frame = matches!(viewer.displayed, DisplayedImage::None)
        || (viewer.pending_since.is_some() && matches!(&anim_msg, AnimMessage::FrameAllocated(..)));

    let (task, allocation) = viewer.anim_player.update(anim_msg, viewer.nav.current());

    if let Some(alloc) = allocation {
        let size = alloc.size();
        if is_first_frame && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
            viewer.zoom = compute_zoom(zoom_mode, size.width, size.height, viewport);
            viewer.pan = (0.0, 0.0);
        }
        viewer.displayed = DisplayedImage::Full {
            allocation: alloc,
            original_size: (size.width, size.height),
        };
        viewer.displayed_path = Some(viewer.nav.current().to_path_buf());
        viewer.pending_since = None;
    }

    // A pending move onto a GIF resolves once its decode lands.
    Task::batch([task.map(AppMessage::Anim), resolve_pending_nav(app)])
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
            &mut app,
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
            &mut app,
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
            &mut app,
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
        let _ = update(&mut app, Message::FileSizeProbed("a.png".into(), 4096));
        assert_eq!(app.viewer().unwrap().current_file_size, Some(4096));
    }

    #[test]
    fn a_stale_file_size_probe_is_ignored() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::FileSizeProbed("b.png".into(), 4096));
        assert_eq!(app.viewer().unwrap().current_file_size, None);
    }

    #[test]
    fn resort_replaces_the_file_order() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(
            &mut app,
            Message::Resorted(vec!["c.png".into(), "b.png".into(), "a.png".into()]),
        );
        assert_eq!(app.viewer().unwrap().nav.files()[0], PathBuf::from("c.png"));
    }

    #[test]
    fn spinner_tick_changes_nothing() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app, Message::SpinnerTick);
        assert_eq!(app.viewer().unwrap().nav.cursor(), 0);
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
            &mut app,
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
