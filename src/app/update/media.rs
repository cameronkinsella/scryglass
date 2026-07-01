use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::media::MediaError;
use crate::media::animation::AnimatedImage;
use crate::media::pipeline::{ThumbUrgency, thumb_key};
use crate::media::store::{AnimRam, ImageKey, RamImage, Tier};
use crate::ui::image_surface::Keepalive;

use crate::app::state::{Session, Thumb};

/// How one tile production ended.
#[derive(Debug, Clone)]
pub enum TileOutcome {
    /// Uploaded and ready to install.
    Ready(Keepalive),
    /// Bailed before the resample: the view moved to another level.
    Canceled,
    /// The resample or upload failed. The next demand pass retries.
    Failed,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// A still decoded to RAM, from a store decode job. Install it in the store
    /// (which mints the texture next) and show it if it is the on-screen image.
    /// The RAM source is boxed to keep this routing message small.
    Decoded {
        key: ImageKey,
        path: PathBuf,
        ram: Box<RamImage>,
        thumb: Option<Thumb>,
    },
    /// A store upload finished: install the texture at its tier and swap the
    /// shared cell. Every window leasing this image sees it sharpen.
    TextureReady {
        key: ImageKey,
        tier: Tier,
        texture: Keepalive,
    },
    /// A tile production finished: install it in the resident pyramid, or
    /// release its claim. `pyramid` is the pyramid it was cut for, so a
    /// production outliving a re-mint never installs into the wrong grid.
    TileReady {
        key: ImageKey,
        tile: crate::media::tiles::TileKey,
        outcome: TileOutcome,
        pyramid: Keepalive,
    },
    /// A zoom gesture's settle timer fired: run the tile demand pass if no
    /// later zoom change superseded it.
    TilesSettled {
        epoch: u64,
    },
    /// An exact-scale tile finished: install it in the pyramid's exact
    /// layer for `target`, or just release its claim on `None`.
    ExactReady {
        key: ImageKey,
        target: (u32, u32),
        tile: crate::media::tiles::TileKey,
        texture: Option<Keepalive>,
        pyramid: Keepalive,
    },
    /// An upload could not reach the GPU after retries; clear the pending mark so
    /// a later pass can try again.
    MintFailed {
        key: ImageKey,
    },
    /// A store decode produced no still: cancelled by a newer navigation, a real
    /// decode failure, or a file that vanished.
    DecodeFailed {
        key: ImageKey,
        path: PathBuf,
        err: MediaError,
    },
    /// A store decode turned out to be an animation. The still store forgets the
    /// key; the frames are registered in the shared animation store instead, where
    /// other windows share them. `decode_time` feeds that store's dynamic evict.
    AnimDecoded {
        key: ImageKey,
        path: PathBuf,
        anim: Arc<AnimatedImage>,
        decode_time: Duration,
        thumb: Option<Thumb>,
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
        original_size: (u32, u32),
        texture: Option<Keepalive>,
    },
    /// Debounced after navigation settles on a view-res neighbor: promote its
    /// lease to a full-res texture so zoom is crisp, unless navigation moved on.
    PromoteCurrent(PathBuf),
    Resorted(Vec<PathBuf>),
    SpinnerTick,
}
use iced::Task;

use crate::anim::AnimMessage;
use crate::app::state::DisplayedImage;
use crate::app::update::{
    complete_navigation, fire_rotate, fire_thumbnailer, resolve_pending_nav, run_jobs, show_loaded,
    show_placeholder,
};
use crate::app::viewer_math::compute_zoom;
use crate::app::{Message as AppMessage, Shared, Window};
use crate::components::filmstrip;
use crate::config::ZoomMode;
use crate::media::pipeline::Lane;

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Decoded {
            key,
            path,
            ram,
            thumb,
        } => {
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let original_size = ram.original_size;
            // Install the RAM source; the store answers with the upload job that
            // mints the texture at the tier this image is wanted.
            let outcome = shared.store.on_decoded(key, *ram);
            if let Some(viewer) = win.viewer_mut() {
                viewer.in_flight.remove(&path);
                if let Some(thumb) = thumb {
                    let cost = thumb.byte_cost();
                    shared
                        .thumbs
                        .insert(thumb_key(&viewer.source, &path), thumb, cost);
                }
                // Put it on screen now; the texture lands shortly via
                // TextureReady, and the blur stands in until it does.
                if viewer.nav.current() == path {
                    show_loaded(viewer, &path, original_size, zoom_mode, viewport);
                }
            }
            let upload = run_jobs(outcome.jobs, &pipeline, Lane::Current, viewport);
            Task::batch([upload, resolve_pending_nav(win, shared)])
        }

        Message::TextureReady { key, tier, texture } => {
            let viewport = win.viewport_size;
            let zoom_mode = shared.config.zoom_mode;
            let pipeline = shared.pipeline.clone();
            let tiled = texture.tiles().is_some();
            // Swap the shared cell; every window leasing this image now draws it.
            let outcome = shared.store.on_minted(key.clone(), tier, texture);
            // If the on-screen image is still standing in with its blur, promote it
            // to the full display now that a texture exists. This covers the image
            // that became resident via a shared upload (another window's decode),
            // so no decode of its own ever fired show_loaded.
            if let Some(viewer) = win.viewer_mut() {
                let target = match &viewer.displayed {
                    DisplayedImage::Placeholder(_) => viewer.displayed_path.clone(),
                    // Nothing on screen at all (no thumbnail to stand in): the
                    // cursor names what this window is waiting for. A fresh
                    // window opening an image another window already holds in
                    // RAM runs no decode, so this upload is its only signal.
                    DisplayedImage::None => Some(viewer.nav.current().to_path_buf()),
                    _ => None,
                };
                let target = target.filter(|path| ImageKey::new(&viewer.source, path) == key);
                if let Some(path) = target
                    && let Some(ram) = shared.store.ram(&key)
                {
                    show_loaded(viewer, &path, ram.original_size, zoom_mode, viewport);
                }
            }
            let jobs = run_jobs(outcome.jobs, &pipeline, Lane::Current, viewport);
            if tiled {
                // A freshly minted pyramid is empty: fill its visible set now.
                return Task::batch([jobs, super::media_tasks::fire_tiles(win, shared)]);
            }
            jobs
        }

        Message::MintFailed { key } => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let outcome = shared.store.on_mint_failed(&key);
            run_jobs(outcome.jobs, &pipeline, Lane::Current, viewport)
        }

        Message::TileReady {
            key,
            tile,
            outcome,
            pyramid,
        } => {
            // Install into the shared pyramid, unless it was re-minted while
            // this production flew (a tile cut for another grid would draw
            // misaligned). Every window leasing the image sees the tile on
            // its next frame.
            if let Some(resident) = shared.store.shared(&key)
                && std::sync::Arc::ptr_eq(&resident, &pyramid)
                && let Some(tiles) = resident.tiles()
            {
                tiles.settle(tile);
                match outcome {
                    TileOutcome::Ready(texture) => tiles.insert(tile, texture),
                    // Re-request a canceled tile the view still wants. The
                    // level guard stops cross-window ping-pong.
                    TileOutcome::Canceled if tiles.wanted_lod() == tile.lod => {
                        return super::media_tasks::fire_tiles(win, shared);
                    }
                    TileOutcome::Canceled | TileOutcome::Failed => {}
                }
            }
            Task::none()
        }

        Message::TilesSettled { epoch } => {
            if win.tile_epoch == epoch {
                super::media_tasks::fire_tiles(win, shared)
            } else {
                Task::none()
            }
        }

        Message::ExactReady {
            key,
            target,
            tile,
            texture,
            pyramid,
        } => {
            if let Some(resident) = shared.store.shared(&key)
                && std::sync::Arc::ptr_eq(&resident, &pyramid)
                && let Some(tiles) = resident.tiles()
            {
                tiles.settle_exact(target, tile, texture);
            }
            Task::none()
        }

        Message::AnimDecoded {
            key,
            path,
            anim,
            decode_time,
            thumb,
        } => {
            // Not a still: the still store forgets it, and the leftover still lease drops.
            shared.store.abandon(&key);
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.in_flight.remove(&path);
            viewer.cache.remove(&path);
            if let Some(thumb) = thumb {
                let cost = thumb.byte_cost();
                shared
                    .thumbs
                    .insert(thumb_key(&viewer.source, &path), thumb, cost);
            }
            // Register the frames in the shared animation store and lease them here,
            // so a second window on this GIF shares the one decode and its decay. The
            // request emits a decode job we drop: `on_decoded` resolves the demand
            // from the frames already in hand. If another window decoded it first,
            // `on_decoded` is a no-op and the lease shares those frames.
            let (lease, _) = shared.anim_store.request(
                key.clone(),
                path.clone(),
                viewer.source.clone(),
                Tier::InRam,
            );
            shared.anim_store.on_decoded(
                key,
                AnimRam {
                    frames: anim,
                    decode_time: Some(decode_time),
                },
            );
            viewer.anim_player.insert(path.clone(), lease);
            // Start playback if this GIF is on screen, unless a dormant playback for it
            // is already in place (a re-decode after a shared eviction): that resumes
            // from where it was on its own once the frames are back, rather than
            // restarting from the first frame.
            let play = if viewer.nav.current() == path && !viewer.anim_player.is_active_on(&path) {
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

        Message::DecodeFailed { key, path, err } => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            // Cancelled by a newer navigation: retry it if a holder still wants
            // it. A real failure or an animation: forget it.
            let retry = matches!(err, MediaError::Cancelled);
            let outcome = shared.store.on_decode_failed(&key, retry);
            let retry_jobs = run_jobs(outcome.jobs, &pipeline, Lane::Current, viewport);

            let Some(viewer) = win.viewer_mut() else {
                return retry_jobs;
            };
            viewer.in_flight.remove(&path);
            if retry {
                return retry_jobs;
            }

            let pending_index = viewer.pending_nav;
            let pending_path = pending_index.map(|i| viewer.nav.files()[i].to_path_buf());
            let is_current = viewer.nav.current() == path;
            let is_pending = pending_path.as_deref() == Some(&*path);
            if !is_current && !is_pending {
                return retry_jobs;
            }
            viewer.pending_since = None;
            if is_pending {
                viewer.pending_nav = None;
            }
            // The file vanished (deleted outside the app): drop it and move on
            // instead of erroring. The watcher usually removes it first.
            if !path.exists() {
                viewer.cache.remove(&path);
                shared.thumbs.remove(&thumb_key(&viewer.source, &path));
                viewer.anim_player.remove(&path);
                viewer.failed_thumbs.remove(&path);
                viewer.failed_loads.remove(&path);
                if !viewer.nav.remove(&path) {
                    win.session = Session::Empty;
                    return retry_jobs;
                }
                let cursor = viewer.nav.cursor();
                return Task::batch([retry_jobs, complete_navigation(win, shared, cursor, true)]);
            }
            // The file exists but won't decode (a video renamed to .png, a
            // truncated image). Remember it and show the error in place.
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let message = format!("{name}\n\n{err}");
            viewer.failed_loads.insert(path.clone(), message.clone());

            if is_pending && let Some(index) = pending_index {
                return Task::batch([retry_jobs, complete_navigation(win, shared, index, false)]);
            }
            // The current file failed in place: show the error unless a good
            // image for it is already on screen.
            let already_shown = matches!(viewer.displayed, DisplayedImage::Full { .. })
                && viewer.displayed_path.as_deref() == Some(&*path);
            if !already_shown {
                viewer.displayed = DisplayedImage::Error { message };
                viewer.displayed_path = Some(path.clone());
            }
            retry_jobs
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
                    shared
                        .thumbs
                        .insert(thumb_key(&viewer.source, &path), thumb.clone(), cost);
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

            let mut tasks = fire_thumbnailer(
                &pipeline,
                &shared.thumbs,
                viewer,
                1,
                window_w,
                show_filmstrip,
            );
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
                    &shared.thumbs,
                    viewer,
                    3,
                    window_w,
                    show_filmstrip,
                ));
            }
            Task::batch(tasks)
        }

        Message::ViewRotated {
            path,
            baked,
            original_size,
            texture,
        } => {
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

            let (w, h) = original_size;
            viewer.displayed = DisplayedImage::Full {
                original_size,
                rotated: texture,
            };
            viewer.displayed_rotation = baked;
            viewer.pan = (0.0, 0.0);
            if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }

            fire_rotate(viewer, &shared.store)
        }

        Message::PromoteCurrent(path) => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            // Only promote if navigation is still resting on this image, so a
            // quick pass-through never re-uploads it at full resolution.
            if viewer.displayed_path.as_deref() != Some(&*path) {
                return Task::none();
            }
            // Raise the on-screen image's lease to a full-res texture for crisp
            // zoom; the store re-uploads from the RAM it already holds.
            let Some(lease) = viewer.cache.get(&path) else {
                return Task::none();
            };
            let outcome = shared.store.retarget(lease, Tier::Full);
            run_jobs(outcome.jobs, &pipeline, Lane::Current, viewport)
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

    let (task, ready) = viewer.anim_player.update(anim_msg, viewer.nav.current());

    if let Some((w, h)) = ready {
        if is_first_frame && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
            viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            viewer.pan = (0.0, 0.0);
        }
        viewer.displayed = DisplayedImage::Animated {
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
        assert!(app.shared.thumbs.contains(&thumb_key(
            &crate::media::pipeline::Source::Fs,
            Path::new("a.png")
        )));
        assert!(
            !app.viewer()
                .unwrap()
                .in_flight_thumbs
                .contains(Path::new("a.png"))
        );
    }

    #[test]
    fn texture_ready_puts_a_first_image_on_screen_without_a_thumb() {
        use crate::media::store::RamImage;
        use iced::widget::image::Handle;

        // Another window already decoded a.png: its RAM sits in the shared
        // store, so this window's open fires only an upload, never a decode.
        // With no thumbnail cached nothing stands in on screen, and the
        // upload's landing is this window's one signal to display.
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let source = crate::media::pipeline::Source::Fs;
        let key = ImageKey::new(&source, Path::new("a.png"));
        let (lease, _) =
            app.shared
                .store
                .request(key.clone(), PathBuf::from("a.png"), source, Tier::Full);
        let _ = app.shared.store.on_decoded(
            key.clone(),
            RamImage {
                handle: Handle::from_rgba(2, 2, vec![0u8; 16]),
                original_size: (2, 2),
                decode_time: None,
            },
        );
        app.viewer_mut()
            .unwrap()
            .cache
            .insert(PathBuf::from("a.png"), lease);
        assert!(matches!(
            app.viewer().unwrap().displayed,
            DisplayedImage::None
        ));

        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::TextureReady {
                key,
                tier: Tier::Full,
                texture: crate::ui::image_surface::test_keepalive(),
            },
        );

        let v = app.viewer().unwrap();
        assert!(matches!(v.displayed, DisplayedImage::Full { .. }));
        assert_eq!(v.displayed_path.as_deref(), Some(Path::new("a.png")));
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
    fn a_broken_file_becomes_a_navigable_error_stop() {
        use std::io::Write;

        use tempfile::TempDir;
        // Real files so the not-found backstop doesn't fire. The cursor starts
        // on `a` with a pending move onto the (undecodable) `b`.
        let dir = TempDir::new().unwrap();
        let (a, b) = (dir.path().join("a.png"), dir.path().join("b.png"));
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

        let key = ImageKey::new(&crate::media::pipeline::Source::Fs, &b);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DecodeFailed {
                key,
                path: b.clone(),
                err: crate::media::MediaError::Decode("bad".into()),
            },
        );

        let v = app.viewer().unwrap();
        // The cursor crossed onto the broken file rather than stalling before it,
        // and the file now shows an error instead of nothing.
        assert_eq!(v.nav.cursor(), 1);
        assert!(matches!(v.displayed, DisplayedImage::Error { .. }));
        assert!(v.failed_loads.contains_key(b.as_path()));
        // `dir` (a TempDir) removes itself on drop.
    }
}
