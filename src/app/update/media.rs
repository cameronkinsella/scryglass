use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::media::MediaError;
use crate::media::animation::AnimatedImage;
use crate::media::pipeline::{ThumbUrgency, thumb_key};
use crate::media::store::{AnimRam, ImageKey, RamImage, Tier};
use crate::ui::image_surface::Keepalive;

use crate::app::state::{Session, Thumb, Viewer};

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
    /// layer for `target`, or release its claim on `None`.
    ExactReady {
        key: ImageKey,
        target: (u32, u32),
        tile: crate::media::tiles::TileKey,
        texture: Option<Keepalive>,
        pyramid: Keepalive,
    },
    /// An upload could not reach the GPU after retries. Clear the pending mark so
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
    /// key. The frames are registered in the shared animation store instead, where
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
            let zoom_mode = shared.config.standard.display.zoom_mode;
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let original_size = ram.original_size;
            // Install the RAM source. The store answers with the upload job that
            // mints the texture at the tier this image is wanted.
            let outcome = shared.store.on_decoded(key, *ram);
            if let Some(viewer) = win.viewer_mut() {
                viewer.in_flight.remove(&path);
                // Release the thumb slot too: a cheap-only probe parked it for
                // this decode to fill (fire_thumb), and nothing else clears it.
                viewer.in_flight_thumbs.remove(&path);
                if let Some(thumb) = thumb {
                    let cost = thumb.byte_cost();
                    shared
                        .thumbs
                        .insert(thumb_key(&viewer.source, &path), thumb, cost);
                }
                // Put it on screen now. The texture lands shortly via
                // TextureReady, and the blur stands in until it does.
                if viewer.nav.current() == path {
                    show_loaded(viewer, &path, original_size, zoom_mode, viewport);
                }
            }
            let upload = run_jobs(win.id, outcome.jobs, &pipeline, Lane::Current, viewport);
            Task::batch([upload, resolve_pending_nav(win, shared)])
        }

        Message::TextureReady { key, tier, texture } => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let tiled = texture.tiles().is_some();
            // Swap the shared cell. Every window leasing this image now draws it.
            let outcome = shared.store.on_minted(key.clone(), tier, texture);
            promote_if_waiting(win, shared, &key);
            let rotate = match win.viewer_mut() {
                // A rotated image returning from decay re-derives its override
                Some(viewer) => fire_rotate(viewer, &shared.store),
                None => Task::none(),
            };
            let jobs = run_jobs(win.id, outcome.jobs, &pipeline, Lane::Current, viewport);
            if tiled {
                // A freshly minted pyramid is empty: fill its visible set now.
                return Task::batch([jobs, rotate, super::media_tasks::fire_tiles(win, shared)]);
            }
            Task::batch([jobs, rotate])
        }

        Message::MintFailed { key } => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            let outcome = shared.store.on_mint_failed(&key);
            run_jobs(win.id, outcome.jobs, &pipeline, Lane::Current, viewport)
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
                    TileOutcome::Canceled => {}
                    // A failed production schedules its own paced retry: an
                    // unchanged view runs no ambient demand passes, so
                    // nothing else would re-request the tile on a resting
                    // view. The epoch guard drops it if the view moves on.
                    TileOutcome::Failed => {
                        let epoch = win.tile_epoch;
                        return Task::future(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            AppMessage::Media(Message::TilesSettled { epoch })
                        });
                    }
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
            viewer.in_flight_thumbs.remove(&path);
            viewer.cache.remove(&path);
            if let Some(thumb) = thumb {
                let cost = thumb.byte_cost();
                shared
                    .thumbs
                    .insert(thumb_key(&viewer.source, &path), thumb, cost);
            }
            let dims = (anim.width, anim.height);
            let play = register_animation(
                viewer,
                &mut shared.anim_store,
                key,
                &path,
                AnimRam {
                    frames: anim,
                    decode_time: Some(decode_time),
                },
            );
            // A canvas past the device texture limit decodes fine but no
            // frame can ever upload. Say so instead of leaving the thumbnail
            // with no explanation.
            let too_large = crate::ui::image_surface::max_texture_dim()
                .is_some_and(|max| dims.0 > max || dims.1 > max);
            let toast = if too_large {
                crate::app::update::push_toast(
                    win,
                    shared,
                    crate::components::toasts::ToastKind::Error,
                    format!("Animation is too large to display ({}x{})", dims.0, dims.1),
                )
            } else {
                Task::none()
            };
            Task::batch([play, toast, resolve_pending_nav(win, shared)])
        }

        Message::DecodeFailed { key, path, err } => {
            let viewport = win.viewport_size;
            let pipeline = shared.pipeline.clone();
            // Cancelled by a newer navigation: retry it if a holder still wants
            // it. A real failure or an animation: forget it.
            let retry = matches!(err, MediaError::Cancelled);
            let outcome = shared.store.on_decode_failed(&key, retry);
            let retry_jobs = run_jobs(win.id, outcome.jobs, &pipeline, Lane::Current, viewport);

            let Some(viewer) = win.viewer_mut() else {
                return retry_jobs;
            };
            viewer.in_flight.remove(&path);
            viewer.in_flight_thumbs.remove(&path);
            if retry {
                return retry_jobs;
            }

            let pending = viewer
                .pending_nav
                .and_then(|i| viewer.nav.files().get(i).map(|p| (i, p.to_path_buf())));
            if viewer.pending_nav.is_some() && pending.is_none() {
                // The folder shrank while the move was pending: the target is
                // gone and the move can never resolve.
                viewer.pending_nav = None;
            }
            let is_current = viewer.nav.current() == path;
            let is_pending = pending.as_ref().is_some_and(|(_, target)| *target == *path);
            if !is_current && !is_pending {
                return retry_jobs;
            }
            viewer.pending_since = None;
            if is_pending {
                viewer.pending_nav = None;
            }
            // The file vanished (deleted outside the app): drop it and move on
            // instead of erroring. The watcher usually removes it first. Only
            // filesystem sources qualify: an archive entry's nav path is a name
            // inside the archive, never a file on disk, so a failing entry must
            // become an error stop below instead of silently leaving the list.
            if viewer.is_fs() && !path.exists() {
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

            if is_pending && let Some((index, _)) = pending {
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
            let zoom_mode = shared.config.standard.display.zoom_mode;
            let viewport = win.viewport_size;
            let window_w = win.window_size.width;
            let show_filmstrip = shared.config.standard.chrome.filmstrip;
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
                // Cancelled. While a full decode of this file is in flight the
                // slot stays parked for it (cheap-only probes bail this way by
                // design) and the decode's completion releases it. Otherwise
                // the cancel came from a generation bump, possibly another
                // window's, which never clears this window's set: release the
                // slot so the pump can re-queue the path instead of skipping
                // it forever. A duplicate job after a same-window re-fire is
                // bounded waste, a wedged slot is not.
                Err(MediaError::Cancelled) => {
                    if !viewer.in_flight.contains(&path) {
                        viewer.in_flight_thumbs.remove(&path);
                    }
                }
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
            let show_filmstrip = shared.config.standard.chrome.filmstrip;
            let pipeline = shared.pipeline.clone();
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            replace_files_keeping_pending(viewer, files);

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
                tasks.push(filmstrip::scroll_strip(viewer, window_id, offset));
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
            let zoom_mode = shared.config.standard.display.zoom_mode;
            let viewport = win.viewport_size;
            // A failed upload leaves the previous texture and rotation state
            // in place. Marking the rotation baked without pixels would draw
            // the unrotated texture into swapped geometry, and the mismatch
            // that remains here lets a later pass retry instead.
            let Some(texture) = texture else {
                return Task::none();
            };
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
                rotated: Some(texture),
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
            let window = win.id;
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
            // zoom. The store re-uploads from the RAM it already holds, and the
            // renewal unparks an entry whose uploads kept failing.
            let Some(lease) = viewer.cache.get(&path) else {
                return Task::none();
            };
            let outcome = shared.store.renew(lease, Tier::Full);
            run_jobs(window, outcome.jobs, &pipeline, Lane::Current, viewport)
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
/// If this window's view is still standing in with its blur (or nothing at
/// all) for exactly the image behind `key`, promote it to the full display
/// now that a texture exists. This covers the image that became resident via
/// a shared upload (another window's decode), so no decode of this window's
/// own ever fired show_loaded. The store answers one window's upload job,
/// and the router sweeps every other window through here on its completion.
pub(crate) fn promote_if_waiting(win: &mut Window, shared: &Shared, key: &ImageKey) {
    let viewport = win.viewport_size;
    let zoom_mode = shared.config.standard.display.zoom_mode;
    let Some(viewer) = win.viewer_mut() else {
        return;
    };
    let target = match &viewer.displayed {
        DisplayedImage::Placeholder(_) => viewer.displayed_path.clone(),
        // Nothing on screen at all (no thumbnail to stand in): the cursor
        // names what this window is waiting for. A fresh window opening an
        // image another window already holds in RAM runs no decode, so
        // this upload is its only signal.
        DisplayedImage::None => Some(viewer.nav.current().to_path_buf()),
        _ => None,
    };
    let target = target.filter(|path| ImageKey::new(&viewer.source, path) == *key);
    if let Some(path) = target
        && let Some(ram) = shared.store.ram(key)
    {
        show_loaded(viewer, &path, ram.original_size, zoom_mode, viewport);
    }
}

/// Register decoded frames in the shared animation store and lease them into
/// this window, so a second window on this GIF shares the one decode and its
/// decay. The request's decode job is dropped: `on_decoded` resolves the
/// demand from the frames already in hand. If another window decoded it
/// first, `on_decoded` is a no-op and the lease shares those frames.
/// Playback starts if the GIF is on screen, unless a dormant playback is
/// already in place (a re-decode after a shared eviction): that resumes from
/// where it was on its own once the frames are back. The AnimDecoded core,
/// shared with the orphaned-completion path so a closed window's decode
/// lands for whichever window waits on it.
pub(crate) fn register_animation(
    viewer: &mut Viewer,
    anim_store: &mut crate::media::store::Store<crate::media::store::Anim>,
    key: ImageKey,
    path: &std::path::Path,
    ram: AnimRam,
) -> Task<AppMessage> {
    let (lease, _) = anim_store.request(
        key.clone(),
        path.to_path_buf(),
        viewer.source.clone(),
        Tier::InRam,
    );
    anim_store.on_decoded(key, ram);
    viewer.anim_player.insert(path.to_path_buf(), lease);
    if viewer.nav.current() == path && !viewer.anim_player.is_active_on(path) {
        viewer
            .anim_player
            .try_start_from_cache(path)
            .map(|t| t.map(AppMessage::Anim))
            .unwrap_or_else(Task::none)
    } else {
        Task::none()
    }
}

/// Swap in a reordered file list, keeping a pending move aimed at the same
/// file. The old index means nothing in the new order, and a target no longer
/// listed abandons the move.
pub(crate) fn replace_files_keeping_pending(viewer: &mut Viewer, files: Vec<PathBuf>) {
    let pending = viewer
        .pending_nav
        .and_then(|i| viewer.nav.files().get(i).cloned());
    viewer.nav.replace_files(files);
    viewer.pending_nav = pending.and_then(|p| viewer.nav.files().iter().position(|f| *f == p));
}

pub(crate) fn update_anim(
    win: &mut Window,
    shared: &mut Shared,
    anim_msg: AnimMessage,
) -> Task<AppMessage> {
    let zoom_mode = shared.config.standard.display.zoom_mode;
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
    use crate::app::test_support::{empty_app, thumb, viewing_app};

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
    fn a_failed_rotate_upload_leaves_the_view_untouched() {
        let mut app = viewing_app(&["a.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.displayed = DisplayedImage::Full {
                original_size: (4, 2),
                rotated: None,
            };
            v.rotation = 1;
            v.displayed_rotation = 0;
            v.zoom = 0.5;
            v.pan = (12.0, -3.0);
            v.manual_zoom = true;
        }
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ViewRotated {
                path: "a.png".into(),
                baked: 1,
                original_size: (2, 4),
                texture: None,
            },
        );
        // The rotation stays unbaked so a later pass retries, and the view
        // (size, pan, zoom) never moves under the user.
        let v = app.viewer().unwrap();
        assert_eq!(v.displayed_rotation, 0);
        assert_eq!(v.pan, (12.0, -3.0));
        assert_eq!(v.zoom, 0.5);
        assert!(matches!(
            v.displayed,
            DisplayedImage::Full {
                original_size: (4, 2),
                rotated: None
            }
        ));
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
    fn a_cancelled_thumb_keeps_its_slot_while_a_decode_owns_it() {
        // The cheap-only probe bailed with Cancelled because the in-flight
        // full decode derives the thumbnail. The slot stays parked for it.
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.in_flight.insert("b.png".into());
            v.in_flight_thumbs.insert("b.png".into());
        }
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
        assert!(v.in_flight_thumbs.contains(Path::new("b.png")));
        assert!(!v.failed_thumbs.contains(Path::new("b.png")));
    }

    #[test]
    fn a_cancelled_thumb_with_no_decode_releases_its_slot() {
        // Another window's navigation bumped the shared thumb generation, so
        // the job bailed. This window never cleared its own set: the slot must
        // release here or the cell wedges blank forever.
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
        assert!(!v.in_flight_thumbs.contains(Path::new("b.png")));
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

    #[test]
    fn a_finished_decode_releases_the_thumb_slot() {
        use crate::media::store::RamImage;
        use iced::widget::image::Handle;

        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let source = crate::media::pipeline::Source::Fs;
        let key = ImageKey::new(&source, Path::new("b.png"));
        let (lease, _) =
            app.shared
                .store
                .request(key.clone(), PathBuf::from("b.png"), source, Tier::Full);
        {
            let v = app.viewer_mut().unwrap();
            v.cache.insert(PathBuf::from("b.png"), lease);
            v.in_flight.insert("b.png".into());
            // The cheap-only probe parked this slot for the decode to fill.
            v.in_flight_thumbs.insert("b.png".into());
        }
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decoded {
                key,
                path: "b.png".into(),
                ram: Box::new(RamImage {
                    handle: Handle::from_rgba(2, 2, vec![0u8; 16]),
                    original_size: (2, 2),
                    decode_time: None,
                }),
                thumb: Some(thumb(2, 2)),
            },
        );
        let v = app.viewer().unwrap();
        assert!(!v.in_flight.contains(Path::new("b.png")));
        assert!(!v.in_flight_thumbs.contains(Path::new("b.png")));
    }

    #[test]
    fn a_failed_decode_releases_the_thumb_slot() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.in_flight.insert("b.png".into());
            v.in_flight_thumbs.insert("b.png".into());
        }
        let key = ImageKey::new(&crate::media::pipeline::Source::Fs, Path::new("b.png"));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DecodeFailed {
                key,
                path: "b.png".into(),
                err: crate::media::MediaError::Decode("bad".into()),
            },
        );
        let v = app.viewer().unwrap();
        assert!(!v.in_flight.contains(Path::new("b.png")));
        assert!(!v.in_flight_thumbs.contains(Path::new("b.png")));
    }

    #[test]
    fn an_archive_entry_that_fails_to_decode_stays_listed_with_an_error() {
        use std::io::Write;

        use tempfile::TempDir;

        // A real zip, since ArchiveIndex only builds from one. Entry contents
        // never decode here, so junk bytes are enough.
        let dir = TempDir::new().unwrap();
        let zip_path = dir.path().join("photos.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for name in ["a.png", "b.png"] {
            writer.start_file(name, options).unwrap();
            writer.write_all(b"not really a png").unwrap();
        }
        writer.finish().unwrap();

        let index =
            std::sync::Arc::new(crate::media::archive::ArchiveIndex::open(&zip_path).unwrap());
        let entries = index.image_entries();
        let start = entries[0].clone();
        let nav = crate::nav::Nav::new(entries, &start).unwrap();
        let viewer = crate::app::state::Viewer::new(
            nav,
            crate::media::pipeline::Source::Archive(index),
            crate::anim::AnimPlayer::new(),
        );
        let mut app = empty_app();
        app.window.session = Session::Viewing(Box::new(viewer));

        let entry = app.viewer().unwrap().nav.current().to_path_buf();
        let key = ImageKey::new(&app.viewer().unwrap().source, &entry);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::DecodeFailed {
                key,
                path: entry.clone(),
                err: crate::media::MediaError::Decode("bad".into()),
            },
        );

        let v = app.viewer().unwrap();
        // The entry name never exists on disk, but it was not deleted: it must
        // stay listed and show an error stop, not silently vanish.
        assert_eq!(v.nav.len(), 2);
        assert!(v.failed_loads.contains_key(entry.as_path()));
        assert!(matches!(v.displayed, DisplayedImage::Error { .. }));
    }

    #[test]
    fn a_resort_remaps_a_pending_move_to_the_same_file() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        // Aimed at b.png, which the resort moves to index 2.
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resorted(vec!["c.png".into(), "a.png".into(), "b.png".into()]),
        );
        assert_eq!(app.viewer().unwrap().pending_nav, Some(2));
    }

    #[test]
    fn a_resort_abandons_a_pending_move_whose_target_vanished() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resorted(vec!["a.png".into(), "c.png".into()]),
        );
        assert!(app.viewer().unwrap().pending_nav.is_none());
    }
}
