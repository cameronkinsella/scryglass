//! Store-driven loads: decode, upload, and prefetch task construction.

use std::path::{Path, PathBuf};

use iced::widget::image::Handle;
use iced::{Size, Task, window};

use crate::app::state::{Thumb, Viewer};
use crate::app::{MediaMessage, Message};
use crate::config::PrefetchVram;
use crate::media::DecodedMedia;
use crate::media::pipeline::{Lane, Pipeline};
use crate::media::store::{Anim, ImageKey, Job, RamImage, Store, Tier};

use super::{fit_zoom, lane_for, prefetch_want, submit_and_wait, view_target};

/// Caps how many prefetch downscales run at once, so rapid navigation through
/// fresh neighbors cannot saturate the CPU with resizes.
static RESIZE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Downscale a full-res RGBA handle to `target`, for a prefetch neighbor's
/// smaller GPU texture. Returns the original handle when it already fits.
/// Downscales through the exact CPU port of the display shader with the live
/// kernel, so the neighbor's view-res copy is indistinguishable from the
/// full-res it promotes to, the same way a demote's GPU-baked copy is.
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
    // Only a target covering both axes needs no work. An either-axis test
    // would hand a barely-over-limit substrate through whole when rounding
    // lands its minor axis at full size.
    if target.0 >= *width && target.1 >= *height {
        return handle.clone();
    }
    let resized = crate::media::resample::downscale(
        pixels.as_ref(),
        (*width, *height),
        target,
        crate::ui::image_surface::current_kernel(),
    );
    Handle::from_rgba(target.0, target.1, resized)
}

/// Claim `path` from the store at `want` for this window, leasing it into the
/// cache, and fire whatever decode or upload the store asks for. The store dedups
/// across windows: a file another window already decoded is shared, not redone.
/// Already-leased here, it only raises the tier if this request wants more.
/// Animations and videos keep their own paths.
pub(crate) fn fire_load(
    window: window::Id,
    store: &mut Store,
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    path: PathBuf,
    want: Tier,
    view: Size,
) -> Task<Message> {
    if viewer.anim_player.has_cached(&path) || crate::video::is_video(&path) {
        return Task::none();
    }
    let lane = lane_for(want);
    if let Some(lease) = viewer.cache.get(&path) {
        // Keep the higher demand, and reconcile even when it is unchanged:
        // the touch heals an entry whose completion message was lost, and
        // renewal unparks one whose uploads kept failing.
        let outcome = store.renew(lease, want.max(lease.want()));
        return run_jobs(window, outcome.jobs, pipeline, lane, view);
    }
    let key = ImageKey::new(&viewer.source, &path);
    let (lease, outcome) = store.request(key, path.clone(), viewer.source.clone(), want);
    // Mark it loading only when a decode is actually firing: that decode produces
    // a thumbnail and clears this when it lands. A request sharing another
    // window's already-resident image runs no decode, so marking it would leave
    // it stuck here and the background thumbnailer would skip it forever.
    if outcome
        .jobs
        .iter()
        .any(|job| matches!(job, Job::Decode { .. }))
    {
        viewer.in_flight.insert(path.clone());
    }
    viewer.cache.insert(path, lease);
    run_jobs(window, outcome.jobs, pipeline, lane, view)
}

/// If `path`'s decoded frames are resident in the shared animation store (this
/// window's own lease, or another window's), lease them into this window and start
/// playback, with no decode. This is the animation counterpart to `fire_load`
/// reusing a still already resident in the store. `None` means the frames are not
/// resident anywhere, so the caller decodes through the still path, which
/// re-discovers the animation and registers it.
pub(crate) fn try_start_shared_anim(
    anim_store: &mut Store<Anim>,
    viewer: &mut Viewer,
    path: &Path,
) -> Option<Task<crate::anim::AnimMessage>> {
    if viewer.anim_player.has_cached(path) {
        // Frames resident (this window's lease, or shared from another window):
        // re-assert full demand so a restore after decay re-pins them.
        if let Some(lease) = viewer.anim_player.lease(path) {
            anim_store.retarget(lease, Tier::InRam);
        }
    } else {
        // No resident lease here, or a stale one whose frames were evicted: drop it,
        // then reuse another window's resident frames if there are any, else give up
        // so the caller decodes through the still path.
        viewer.anim_player.remove(path);
        let key = ImageKey::new(&viewer.source, path);
        anim_store.ram(&key)?;
        let (lease, _) =
            anim_store.request(key, path.to_path_buf(), viewer.source.clone(), Tier::InRam);
        viewer.anim_player.insert(path.to_path_buf(), lease);
    }
    // A dormant (or running) playback for this GIF resumes from where it is on its
    // own once the frames are pinned. Do not restart it from the first frame.
    if viewer.anim_player.is_active_on(path) {
        return Some(Task::none());
    }
    viewer.anim_player.try_start_from_cache(path)
}

/// Turn the store's pending [`Job`]s into async tasks. A decode reads and decodes
/// from disk (or finds an animation). An upload pushes RAM to the GPU. Each
/// reports back so the store can install the result and swap the shared cell.
pub(crate) fn run_jobs(
    window: window::Id,
    jobs: Vec<Job>,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
) -> Task<Message> {
    run_jobs_at(window, jobs, pipeline, lane, view, None)
}

/// Like [`run_jobs`], but for a demote of the on-screen image `zoom` is its current
/// zoom, so its view-res copy is sized to what is actually displayed rather than the
/// fit zoom it opened at. `None` (prefetch and fresh loads) uses the fit zoom.
pub(crate) fn run_jobs_at(
    window: window::Id,
    jobs: Vec<Job>,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
    zoom: Option<f32>,
) -> Task<Message> {
    Task::batch(
        jobs.into_iter()
            .map(|job| run_job(window, job, pipeline, lane, view, zoom)),
    )
}

fn run_job(
    window: window::Id,
    job: Job,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
    zoom_override: Option<f32>,
) -> Task<Message> {
    match job {
        Job::Decode { key, path, source } => {
            let generation = pipeline.generation_for(window);
            let load = pipeline.load_for(
                window,
                source,
                path.clone(),
                pipeline.decode_opts(),
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
            Task::perform(timed, |x| x).map(move |(result, decode_time)| match result {
                Ok(DecodedMedia::Static(img)) => {
                    let thumb = img.thumbnail.map(Thumb::from);
                    let handle = Handle::from_rgba(img.width, img.height, img.pixels);
                    Message::Media(MediaMessage::Decoded {
                        key: key.clone(),
                        path: path.clone(),
                        ram: Box::new(RamImage {
                            handle,
                            original_size: img.original_size,
                            decode_time: Some(decode_time),
                        }),
                        thumb,
                    })
                }
                Ok(DecodedMedia::Animated(anim)) => {
                    // Frames allocate at display time. Only the thumb needs a
                    // handle here. The store forgets this key (it is not a still).
                    let thumb = anim.thumbnail.clone().map(Thumb::from);
                    Message::Media(MediaMessage::AnimDecoded {
                        key: key.clone(),
                        path: path.clone(),
                        anim,
                        decode_time,
                        thumb,
                    })
                }
                Err(err) => Message::Media(MediaMessage::DecodeFailed {
                    key: key.clone(),
                    path: path.clone(),
                    err,
                }),
            })
        }
        Job::Upload {
            key,
            tier,
            ram,
            source,
        } => Task::future(async move {
            // The upload thread exists once any window draws a frame, which a
            // window revealed late (a maximized relaunch) delays well past any
            // fixed retry count. Wait for it, bounded only as a backstop.
            for _ in 0..3750 {
                if crate::ui::image_surface::upload_ready() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            }
            // Retry briefly past readiness for the transient failure modes.
            for _ in 0..30 {
                let zoom = zoom_override.unwrap_or_else(|| fit_zoom(ram.original_size, view));
                let texture = if tier >= Tier::Full {
                    full_texture(&ram.handle, ram.original_size, source.clone(), zoom).await
                } else {
                    view_res_texture(source.clone(), &ram.handle, ram.original_size, zoom).await
                };
                if let Some(texture) = texture {
                    return Message::Media(MediaMessage::TextureReady { key, tier, texture });
                }
                tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            }
            Message::Media(MediaMessage::MintFailed { key })
        }),
    }
}

/// The Full-tier resident for a decoded still: one full-res texture when the
/// substrate fits the device limit, else a tile pyramid the demand passes
/// fill from the RAM source as the view moves. The pyramid keeps a
/// view-quality base layer beneath its tiles (the texture it is promoted
/// from, or one derived here), so it is never blank while tiles arrive.
async fn full_texture(
    handle: &Handle,
    original_size: (u32, u32),
    source: Option<crate::ui::image_surface::Keepalive>,
    zoom: f32,
) -> Option<crate::ui::image_surface::Keepalive> {
    if let Handle::Rgba { width, height, .. } = handle
        && width.max(height) > &crate::media::registry::MAX_TEXTURE_DIM
    {
        // A failed base upload fails the mint, so the caller's retry loop
        // covers the upload-thread warmup race and a pyramid is never minted
        // without its never-blank layer.
        let base = match source {
            Some(view) => view,
            None => upload_at_res(handle, original_size, zoom, false).await?,
        };
        return Some(crate::ui::image_surface::ResidentImage::tiled(
            (*width, *height),
            base,
        ));
    }
    upload_at_res(handle, original_size, 1.0, true).await
}

/// Produce the view-resolution texture for `zoom`. When a full-res texture is still
/// resident (`source`, a demote), the copy is baked from it on the GPU through the
/// display shader, so it looks like the full-res view it replaces. Otherwise (a
/// fresh prefetch, or if that render path is unavailable) it falls back to a CPU
/// downscale of the RAM.
async fn view_res_texture(
    source: Option<crate::ui::image_surface::Keepalive>,
    handle: &Handle,
    original_size: (u32, u32),
    zoom: f32,
) -> Option<crate::ui::image_surface::Keepalive> {
    if let Some(src) = source {
        let target = view_target(
            original_size,
            zoom,
            crate::ui::image_surface::current_scale_factor(),
        );
        if let Some(rx) = crate::ui::image_surface::submit_render_downscale(src, target)
            && let Ok(texture) = rx.await
        {
            return Some(texture);
        }
    }
    upload_at_res(handle, original_size, zoom, false).await
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
    if full {
        return submit_and_wait(handle.clone()).await;
    }
    // Bound concurrent derives so spamming through new neighbors never
    // saturates the CPU, the upload thread, or transient VRAM.
    let _permit = RESIZE_GATE.acquire().await.ok();
    let scale_factor = crate::ui::image_surface::current_scale_factor();
    let target = view_target(original_size, zoom, scale_factor);
    let covers = target.0 >= original_size.0 && target.1 >= original_size.1;
    // The GPU bake renders the copy through the display shader, trading a
    // transient full-size texture for the whole CPU resample. Identical
    // pixels either way (the CPU path is the shader's exact port), so this
    // is purely the prefetch_scaler resource trade. Falls back to the CPU
    // when the full decode exceeds a texture or the GPU path fails.
    if !covers
        && PREFETCH_GPU_BAKE.load(std::sync::atomic::Ordering::Relaxed)
        && let Some(full_texture) = submit_and_wait(handle.clone()).await
        && let Some(rx) = crate::ui::image_surface::submit_render_downscale(full_texture, target)
        && let Ok(view) = rx.await
    {
        return Some(view);
    }
    let h = handle.clone();
    let gpu_handle = tokio::task::spawn_blocking(move || {
        crate::platform::run_below_normal(|| downscale(&h, target))
    })
    .await
    .unwrap_or_else(|_| handle.clone());
    submit_and_wait(gpu_handle).await
}

/// Mirrors the `prefetch_scaler` config so the prefetch call graph needs no
/// config handle, the same pattern the kernel and RAM budget use.
static PREFETCH_GPU_BAKE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Adopt the configured prefetch scaler (boot and config changes).
pub(crate) fn set_prefetch_scaler(scaler: crate::config::PrefetchScaler) {
    PREFETCH_GPU_BAKE.store(
        scaler == crate::config::PrefetchScaler::Gpu,
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Warm the prefetch window around the cursor, each neighbor leased at the tier
/// the `prefetch_vram` setting asks for (full-res, view-res, or RAM only).
pub(crate) fn fire_prefetch(
    window: window::Id,
    store: &mut Store,
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    depth: usize,
    view: Size,
    prefetch_vram: PrefetchVram,
) -> Vec<Task<Message>> {
    let want = prefetch_want(prefetch_vram);
    viewer
        .nav
        .peek_around(depth)
        .into_iter()
        .map(|p| fire_load(window, store, pipeline, viewer, p, want, view))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;
    use crate::media::pipeline::Source;

    #[test]
    fn fire_load_never_lowers_a_lease_demand() {
        use crate::app::test_support::cache_image;
        use crate::media::store::Tier;

        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        let window = app.window.id;
        let pipeline = app.shared.pipeline.clone();
        let viewer = app.window.viewer_mut().unwrap();
        let _ = fire_load(
            window,
            &mut app.shared.store,
            &pipeline,
            viewer,
            "a.png".into(),
            Tier::View,
            Size::new(800.0, 600.0),
        );
        // A prefetch-tier touch on a full-res lease reconciles but keeps it.
        let lease = app
            .viewer()
            .unwrap()
            .cache
            .get(std::path::Path::new("a.png"));
        assert_eq!(lease.unwrap().want(), Tier::Full);
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

    #[test]
    fn two_windows_share_one_gif_decode_and_its_decay() {
        use crate::anim::AnimPlayer;
        use crate::app::state::Viewer;
        use crate::media::animation::{AnimatedImage, RawFrame};
        use crate::media::store::{Anim, AnimRam};
        use crate::nav::Nav;
        use std::sync::Arc;

        fn frames() -> Arc<AnimatedImage> {
            Arc::new(AnimatedImage {
                width: 2,
                height: 2,
                frames: vec![RawFrame {
                    left: 0,
                    top: 0,
                    width: 2,
                    height: 2,
                    pixels: vec![0u8; 16],
                    dispose: gif::DisposalMethod::Keep,
                    delay: std::time::Duration::from_millis(100),
                }],
                thumbnail: None,
            })
        }

        fn viewer_on(path: &str) -> Viewer {
            let p = PathBuf::from(path);
            let nav = Nav::new(vec![p.clone()], &p).unwrap();
            Viewer::new(nav, Source::Fs, AnimPlayer::new())
        }

        let mut anim_store: Store<Anim> = Store::default();
        let path = Path::new("a.gif");
        let key = ImageKey::new(&Source::Fs, path);

        // Window A decodes the GIF and holds the lease, as the AnimDecoded handler does.
        let mut a = viewer_on("a.gif");
        let (lease_a, _) =
            anim_store.request(key.clone(), path.to_path_buf(), Source::Fs, Tier::InRam);
        anim_store.on_decoded(
            key.clone(),
            AnimRam {
                frames: frames(),
                decode_time: None,
            },
        );
        a.anim_player.insert(path.to_path_buf(), lease_a);
        assert!(anim_store.ram(&key).is_some());

        // Window B opens the same GIF: it reuses A's resident frames with no decode.
        let mut b = viewer_on("a.gif");
        assert!(try_start_shared_anim(&mut anim_store, &mut b, path).is_some());
        assert!(b.anim_player.has_cached(path));

        // One allocation, not two: through the real wiring both windows' leases read
        // the very same frames by pointer, so opening the GIF twice did not duplicate
        // it in memory.
        let frames_a = a.anim_player.lease(path).unwrap().texture().unwrap();
        let frames_b = b.anim_player.lease(path).unwrap().texture().unwrap();
        assert!(Arc::ptr_eq(&frames_a, &frames_b));

        // A backgrounds: decay lowers its demand to evicted. The frames survive
        // because B still holds them, so A keeps showing the GIF (decay state shared).
        anim_store.retarget(a.anim_player.lease(path).unwrap(), Tier::Evicted);
        let _ = anim_store.pump();
        assert!(
            anim_store.ram(&key).is_some(),
            "B still holds the GIF, so its frames stay resident"
        );
        assert!(a.anim_player.has_cached(path));

        // B backgrounds too: the last demand is gone, so the frames free and both
        // windows now derive nothing.
        anim_store.retarget(b.anim_player.lease(path).unwrap(), Tier::Evicted);
        let _ = anim_store.pump();
        assert!(
            anim_store.ram(&key).is_none(),
            "no window wants the GIF, so its frames free"
        );
        assert!(!a.anim_player.has_cached(path));
    }
}
