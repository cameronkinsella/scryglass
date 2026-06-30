use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use iced::widget::image::Handle;
use iced::{Size, Task};

use crate::app::state::{DisplayedImage, Thumb, Viewer};
use crate::app::viewer_math::compute_zoom;
use crate::app::{MediaMessage, Message, Shared, Window};
use crate::config::{PrefetchVram, ZoomMode};
use crate::media::cache::ImageCache;
use crate::media::pipeline::{Lane, Pipeline, Source, ThumbUrgency, thumb_key};
use crate::media::store::{Anim, ImageKey, Job, RamImage, Store, Tier};
use crate::media::{DecodedMedia, MediaError, ThumbData};

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
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let keepalive = if crate::ui::image_surface::submit_upload(handle.clone(), ready_tx) {
                ready_rx.await.ok()
            } else {
                None
            };
            Message::Media(MediaMessage::ViewRotated {
                path: p.clone(),
                baked,
                original_size: (width, height),
                texture: keepalive,
            })
        })
    })
}

/// Caps how many prefetch downscales run at once, so rapid navigation through
/// fresh neighbors cannot saturate the CPU with resizes.
static RESIZE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// The view-resolution target for an image of `original` size shown at `zoom`
/// (1.0 = full native): the displayed size in physical pixels (logical size times
/// the display `scale_factor`), never upscaled past native. Sizing to the physical
/// display, rather than a fixed headroom, keeps the demoted copy crisp on HiDPI and
/// seamless with the full-res view on the way down. A demoted but visible image
/// targets its current zoom so it stays as crisp as what is on screen; a prefetch
/// neighbor targets its fit zoom, since it is shown fit when navigated to.
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

/// The fit zoom for an image of `original` size in `view`: the uniform scale that
/// makes it fit on both axes, never upscaled. This is what an image shows at when
/// navigated to fresh, so prefetch and restored textures target it.
fn fit_zoom(original: (u32, u32), view: Size) -> f32 {
    let (w, h) = (original.0.max(1) as f32, original.1.max(1) as f32);
    (view.width / w).min(view.height / h).min(1.0)
}

/// Downscale a full-res RGBA handle to `target`, for a prefetch neighbor's smaller
/// GPU texture. Returns the original handle when it already fits.
///
/// Downscales through the exact CPU port of the display shader with the live kernel,
/// so a prefetched neighbor's view-res copy is indistinguishable from the full-res
/// it promotes to, the same way a demote's GPU-baked copy is. A plain resize (a
/// fixed cubic averaged in gamma space) was visibly softer than the shader's
/// linear-light kernel.
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

/// Fire a thumbnail job for `path` unless one is cached, in flight, or
/// known to fail.
pub(crate) fn fire_thumb(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    path: PathBuf,
    urgency: ThumbUrgency,
) -> Task<Message> {
    if thumbs.contains(&thumb_key(&viewer.source, &path))
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

/// Fire a first-frame thumbnail for an archive video from its just-extracted
/// temp `file`, keyed under the archive `entry`. The entry has no real path for
/// FFmpeg, so this reuses the file playback already wrote; `guard` keeps it
/// alive through the decode. Skips when a thumbnail is cached, in flight, or
/// known to fail. Background urgency, since the playing video covers the wait.
pub(crate) fn fire_archive_video_thumb(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    entry: PathBuf,
    file: PathBuf,
    guard: std::sync::Arc<crate::video::TempFileGuard>,
) -> Task<Message> {
    if thumbs.contains(&thumb_key(&viewer.source, &entry))
        || viewer.in_flight_thumbs.contains(&entry)
        || viewer.failed_thumbs.contains(&entry)
    {
        return Task::none();
    }

    viewer.in_flight_thumbs.insert(entry.clone());
    let load = pipeline.load_video_thumb_from_file(file, viewer.source.clone(), entry.clone());
    Task::perform(
        async move {
            // Hold the temp file open until the first-frame decode finishes,
            // even if playback navigated away and dropped its own guard.
            let _guard = guard;
            load.await
        },
        move |result| {
            Message::Media(MediaMessage::ThumbLoaded {
                path: entry.clone(),
                urgency: ThumbUrgency::Background,
                result: result.map(|data| Thumb {
                    handle: Handle::from_rgba(data.width, data.height, data.pixels),
                    size: (data.width, data.height),
                    original_size: data.original_size,
                }),
            })
        },
    )
}

/// Start (or continue) background thumbnailing: up to `chains` jobs from the
/// current [`thumb_focus`].
pub(crate) fn fire_thumbnailer(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    chains: usize,
    viewport_w: f32,
    filmstrip_shown: bool,
) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for _ in 0..chains {
        let (center, range) = thumb_focus(viewer, viewport_w, filmstrip_shown);
        let Some(path) = viewer.next_unthumbed_in(thumbs, center, range) else {
            break;
        };
        tasks.push(fire_thumb(
            pipeline,
            thumbs,
            viewer,
            path,
            ThumbUrgency::Background,
        ));
    }
    tasks
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

/// Claim `path` from the store at `want` for this window, leasing it into the
/// cache, and fire whatever decode or upload the store asks for. The store dedups
/// across windows: a file another window already decoded is shared, not redone.
/// Already-leased here, it only raises the tier if this request wants more.
/// Animations and videos keep their own paths.
pub(crate) fn fire_load(
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
        if lease.want() >= want {
            return Task::none();
        }
        let outcome = store.retarget(lease, want);
        return run_jobs(outcome.jobs, pipeline, lane, view);
    }
    let key = ImageKey::new(&viewer.source, &path);
    let (lease, outcome) = store.request(key, path.clone(), viewer.source.clone(), want);
    // Mark it loading only when a decode is actually firing: that decode produces
    // a thumbnail and clears this when it lands. A request that shares another
    // window's already-resident image runs no decode, so marking it would leave it
    // stuck here and the background thumbnailer would skip it forever, never giving
    // this window a thumbnail.
    if outcome
        .jobs
        .iter()
        .any(|job| matches!(job, Job::Decode { .. }))
    {
        viewer.in_flight.insert(path.clone());
    }
    viewer.cache.insert(path, lease);
    run_jobs(outcome.jobs, pipeline, lane, view)
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
    // own once the frames are pinned; do not restart it from the first frame.
    if viewer.anim_player.is_active_on(path) {
        return Some(Task::none());
    }
    viewer.anim_player.try_start_from_cache(path)
}

/// Turn the store's pending [`Job`]s into async tasks. A decode reads and decodes
/// from disk (or finds an animation); an upload pushes RAM to the GPU. Each
/// reports back so the store can install the result and swap the shared cell.
pub(crate) fn run_jobs(
    jobs: Vec<Job>,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
) -> Task<Message> {
    run_jobs_at(jobs, pipeline, lane, view, None)
}

/// Like [`run_jobs`], but for a demote of the on-screen image `zoom` is its current
/// zoom, so its view-res copy is sized to what is actually displayed rather than the
/// fit zoom it opened at. `None` (prefetch and fresh loads) uses the fit zoom.
pub(crate) fn run_jobs_at(
    jobs: Vec<Job>,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
    zoom: Option<f32>,
) -> Task<Message> {
    Task::batch(
        jobs.into_iter()
            .map(|job| run_job(job, pipeline, lane, view, zoom)),
    )
}

fn run_job(
    job: Job,
    pipeline: &Pipeline,
    lane: Lane,
    view: Size,
    zoom_override: Option<f32>,
) -> Task<Message> {
    match job {
        Job::Decode { key, path, source } => {
            let generation = pipeline.generation();
            let load = pipeline.load(
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
                    let thumb = img.thumbnail.map(|t| Thumb {
                        handle: Handle::from_rgba(t.width, t.height, t.pixels),
                        size: (t.width, t.height),
                        original_size: t.original_size,
                    });
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
                    // Frames allocate at display time; only the thumb needs a
                    // handle here. The store forgets this key (it is not a still).
                    let thumb = anim.thumbnail.as_ref().map(|t| Thumb {
                        handle: Handle::from_rgba(t.width, t.height, t.pixels.clone()),
                        size: (t.width, t.height),
                        original_size: t.original_size,
                    });
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
            // The upload thread is built by the warmup surface; retry briefly so
            // the very first upload, which may race that setup, still lands.
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

/// Caps concurrent tile productions. Each is one small resample, but a demand
/// pass requests a viewport's worth at once and the resampler is itself
/// parallel, so two at a time keeps latency low without saturating the CPU.
static TILE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Request the tiles a tiled on-screen still is missing for its current view:
/// the visible set at the zoom's level, inflated half a tile for pan headroom.
/// A no-op for ordinary stills and for views whose tiles are all resident or
/// in flight, so it is cheap to call after any view change.
pub(crate) fn fire_tiles(win: &Window, shared: &Shared) -> Task<Message> {
    use crate::media::tiles;
    use crate::ui::image_surface::DrawWant;
    let Some(viewer) = win.viewer() else {
        return Task::none();
    };
    // Only a still past the texture limit can be tiled: a cheap gate, so the
    // per-message calls for ordinary images cost no allocation or lookup.
    let true_size = match viewer.displayed.original_size() {
        Some(size) if size.0.max(size.1) > crate::media::registry::MAX_TEXTURE_DIM => size,
        _ => return Task::none(),
    };
    let Some(path) = viewer.displayed_path.as_ref() else {
        return Task::none();
    };
    let key = ImageKey::new(&viewer.source, path);
    let Some(resident) = shared.store.shared(&key) else {
        return Task::none();
    };
    let Some(set) = resident.tiles() else {
        return Task::none();
    };
    let Some(ram) = shared.store.ram(&key) else {
        return Task::none();
    };
    let original = set.original();
    let viewport = win.viewport_size;
    let zoom = if viewer.manual_zoom {
        viewer.zoom
    } else {
        compute_zoom(shared.config.zoom_mode, true_size.0, true_size.1, viewport)
    };
    // The draw stamps the level and scale it actually sampled. Before the
    // first tiled draw, derive them the same way in substrate texels.
    let substrate_zoom = zoom * true_size.0 as f32 / original.0 as f32;
    let scale = match set.draw_want() {
        DrawWant::Unknown => crate::ui::image_surface::current_scale_factor().max(1.0),
        _ => set.draw_scale().max(1.0),
    };
    // A resting view that fits one texture is served by the base alone, at
    // EXACTLY the displayed size: the same one-pass copy the View tier
    // shows, grid-aligned so its single taps are texel-exact. Any size
    // mismatch, however small, samples between texels and softens.
    let max = crate::media::registry::MAX_TEXTURE_DIM as f32;
    let displayed = (
        original.0 as f32 * substrate_zoom * scale,
        original.1 as f32 * substrate_zoom * scale,
    );
    // The placement geometry runs in displayed coordinates, like the draw's.
    let Some((_, src)) = crate::ui::image_display::display_geometry(
        zoom,
        viewer.pan,
        (viewport.width, viewport.height),
        true_size,
    ) else {
        return Task::none();
    };
    if displayed.0 <= max && displayed.1 <= max {
        let target = view_target(original, substrate_zoom, scale);
        // Stamped before the exact-match return, so the cell always names
        // the resting size and never keeps a stale (0, 0) from a tiled
        // excursion. Queued derives for other sizes bail once it is stored.
        set.set_wanted_base(target);
        if set.base().and_then(|base| base.size()) == Some(target) {
            return Task::none();
        }
        let mut jobs = Vec::new();
        // Before the first tiled draw the scale is a process-global guess,
        // so the claim waits for the settle that follows the mint, which
        // derives with this window's stamped scale.
        if !matches!(set.draw_want(), DrawWant::Unknown) && set.try_claim_base(target) {
            jobs.push(refresh_base(
                key.clone(),
                &ram.handle,
                resident.clone(),
                target,
            ));
        }
        // A large exact base takes seconds; tiles sharpen the visible region
        // in the meantime and stop drawing once it lands.
        if let DrawWant::Level(lod) = set.draw_want() {
            jobs.extend(claim_tiles(&key, &ram, &resident, set, src, original, lod));
        }
        return Task::batch(jobs);
    }
    // Past one texture's worth of display, tiles are the only way. No base
    // size is wanted here, which aborts any in-flight derive mid-pass.
    set.set_wanted_base((0, 0));
    let lod = match set.draw_want() {
        DrawWant::Level(lod) => lod,
        DrawWant::BaseOnly | DrawWant::Unknown => tiles::lod_for_zoom(substrate_zoom * scale),
    };
    Task::batch(claim_tiles(&key, &ram, &resident, set, src, original, lod))
}

/// Claim and produce the visible tiles at `lod` that are neither resident nor
/// in flight. Only the prefix one frame can draw is considered at all, so
/// every pass converges on the same tiles and a display larger than the slot
/// budget degrades to the base layer instead of producing and evicting in a
/// loop.
fn claim_tiles(
    key: &ImageKey,
    ram: &RamImage,
    resident: &crate::ui::image_surface::Keepalive,
    set: &crate::ui::image_surface::TileSet,
    src: [f32; 4],
    original: (u32, u32),
    lod: u32,
) -> Vec<Task<Message>> {
    use crate::media::tiles;
    let level = tiles::level_size(original, lod);
    // Queued productions for any other level bail once this is stored.
    set.set_wanted_lod(lod);
    // Half a tile of pan margin on every side.
    let margin = (
        tiles::TILE_SIZE as f32 / (2.0 * level.0 as f32),
        tiles::TILE_SIZE as f32 / (2.0 * level.1 as f32),
    );
    let wanted = [
        src[0] - margin.0,
        src[1] - margin.1,
        src[2] + margin.0,
        src[3] + margin.1,
    ];
    tiles::visible_tiles(wanted, original, lod)
        .take(crate::ui::image_surface::MAX_TILE_DRAWS)
        .filter(|tile| set.try_claim(*tile))
        .map(|tile| produce_tile(key.clone(), &ram.handle, resident.clone(), tile))
        .collect()
}

/// Schedule a debounced tile demand pass for a changing view (zoom, resize,
/// config): it runs after the change rests and a frame has stamped the new
/// placement, so demand always follows real draw geometry.
pub(crate) fn settle_tiles(win: &mut Window) -> Task<Message> {
    if win.viewer().is_none() {
        return Task::none();
    }
    win.tile_epoch += 1;
    let epoch = win.tile_epoch;
    Task::future(async move {
        // Gesture rest threshold, chosen by feel.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        Message::Media(MediaMessage::TilesSettled { epoch })
    })
}

/// Re-derive a tiled still's base layer at `target`: one direct pass from the
/// substrate, exactly how the View tier builds its copy, swapped into the
/// pyramid when it lands.
fn refresh_base(
    key: ImageKey,
    handle: &Handle,
    pyramid: crate::ui::image_surface::Keepalive,
    target: (u32, u32),
) -> Task<Message> {
    let handle = handle.clone();
    Task::future(async move {
        let _permit = RESIZE_GATE.acquire().await.ok();
        // A rest at another size supersedes this derive; bail before paying
        // for a whole-substrate resample nobody will draw.
        if pyramid.tiles().is_none_or(|set| !set.base_wanted(target)) {
            return Message::Media(MediaMessage::BaseReady {
                key,
                texture: None,
                target,
                pyramid,
            });
        }
        // Restart the claim clock past the gate wait, like a tile production.
        if let Some(set) = pyramid.tiles() {
            set.refresh_base_claim(target);
        }
        let watcher = pyramid.clone();
        let derived = tokio::task::spawn_blocking(move || {
            // Half the cores: an exact base for a large display is seconds
            // of kernel taps, and taking every core throttles the whole app.
            BASE_POOL.install(|| {
                // Abandoned row by row once another rest wants a different
                // size, so a superseded derive stops costing immediately.
                let cancel = || watcher.tiles().is_none_or(|set| !set.base_wanted(target));
                downscale_or_cancel(&handle, target, &cancel)
            })
        })
        .await;
        let texture = match derived {
            Ok(Some(derived)) => submit_and_wait(derived).await,
            Ok(None) | Err(_) => None,
        };
        Message::Media(MediaMessage::BaseReady {
            key,
            texture,
            target,
            pyramid,
        })
    })
}

/// [`downscale`], abandoned when `cancel` turns true.
fn downscale_or_cancel(
    handle: &Handle,
    target: (u32, u32),
    cancel: &(dyn Fn() -> bool + Sync),
) -> Option<Handle> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return Some(handle.clone());
    };
    if target.0 >= *width && target.1 >= *height {
        return Some(handle.clone());
    }
    let resized = crate::media::resample::downscale_cancellable(
        pixels.as_ref(),
        (*width, *height),
        target,
        crate::ui::image_surface::current_kernel(),
        cancel,
    )?;
    Some(Handle::from_rgba(target.0, target.1, resized))
}

/// Runs whole-substrate base derives on half the cores, so a seconds-long
/// exact resample never saturates the machine.
static BASE_POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(
            std::thread::available_parallelism()
                .map(|n| (n.get() / 2).max(1))
                .unwrap_or(1),
        )
        .build()
        .expect("static thread pool config is valid")
});

/// Submit `handle` to the upload thread and wait for its keepalive.
async fn submit_and_wait(handle: Handle) -> Option<crate::ui::image_surface::Keepalive> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    if crate::ui::image_surface::submit_upload(handle, ready_tx) {
        ready_rx.await.ok()
    } else {
        None
    }
}

/// Produce and upload one tile of a tiled still. The tile is cut from the RAM
/// substrate with the region resampler, whose border taps read the whole
/// image, so adjacent tiles reassemble seamlessly; the finished tile uploads
/// like any small image.
pub(crate) fn produce_tile(
    key: ImageKey,
    handle: &Handle,
    pyramid: crate::ui::image_surface::Keepalive,
    tile: crate::media::tiles::TileKey,
) -> Task<Message> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return Task::none();
    };
    let (w, h) = (*width, *height);
    let pixels = pixels.clone();
    Task::future(async move {
        let _permit = TILE_GATE.acquire().await.ok();
        // Bail without resampling when the view crossed to another level
        // while this sat queued, or when the tile already landed (an expired
        // claim can admit a queued duplicate).
        if pyramid
            .tiles()
            .is_none_or(|set| set.wanted_lod() != tile.lod || set.get(tile).is_some())
        {
            return Message::Media(MediaMessage::TileReady {
                key,
                tile,
                outcome: crate::app::update::media::TileOutcome::Canceled,
                pyramid,
            });
        }
        // Restart the claim clock now that the work begins: the TTL covers
        // the resample and upload, not the wait behind the gate.
        if let Some(set) = pyramid.tiles() {
            set.refresh_claim(tile);
        }
        let kernel = crate::ui::image_surface::current_kernel();
        let produced = tokio::task::spawn_blocking(move || {
            // Gutter-padded: the payload sits GUTTER deep inside, so the
            // display kernel's edge taps read true neighbor pixels.
            let (region, (tw, th)) = crate::media::tiles::production((w, h), tile);
            let out = crate::media::resample::downscale_region(
                pixels.as_ref(),
                (w, h),
                region,
                (tw, th),
                kernel,
            );
            Handle::from_rgba(tw, th, out)
        })
        .await
        .ok();
        use crate::app::update::media::TileOutcome;
        let outcome = match produced {
            Some(tile_handle) => submit_and_wait(tile_handle)
                .await
                .map_or(TileOutcome::Failed, TileOutcome::Ready),
            None => TileOutcome::Failed,
        };
        Message::Media(MediaMessage::TileReady {
            key,
            tile,
            outcome,
            pyramid,
        })
    })
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
    let gpu_handle = if full {
        handle.clone()
    } else {
        // Bound concurrent downscales so spamming through new neighbors never
        // saturates the CPU with resizes.
        let _permit = RESIZE_GATE.acquire().await.ok();
        let h = handle.clone();
        let scale_factor = crate::ui::image_surface::current_scale_factor();
        tokio::task::spawn_blocking(move || {
            downscale(&h, view_target(original_size, zoom, scale_factor))
        })
        .await
        .unwrap_or_else(|_| handle.clone())
    };
    submit_and_wait(gpu_handle).await
}

/// Warm the prefetch window around the cursor, each neighbor leased at the tier
/// the `prefetch_vram` setting asks for (full-res, view-res, or RAM only).
pub(crate) fn fire_prefetch(
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
        .map(|p| fire_load(store, pipeline, viewer, p, want, view))
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
    fn view_target_sizes_to_the_physical_display() {
        // 4000x3000 fit into 800x600 is a 0.2 zoom. At 100% scaling the copy is the
        // logical size; a 200% display doubles it to stay crisp on the denser panel.
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
        // the texture limit, aspect preserved; tiles carry the rest.
        let max = crate::media::registry::MAX_TEXTURE_DIM;
        assert_eq!(view_target((20000, 10000), 1.0, 1.0), (max, max / 2));
        assert_eq!(view_target((20000, 10000), 0.1, 1.0), (2000, 1000));
    }

    #[test]
    fn prefetch_want_resolves_the_prefetch_vram_mode() {
        // The prefetch tier follows the setting; the current image always asks
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
    fn archive_video_thumb_claims_the_slot_then_skips_when_present() {
        use crate::app::test_support::{cache_thumb, viewing_app};

        let mut app = viewing_app(&["clip.mp4"], 0);
        let entry = PathBuf::from("clip.mp4");
        let file = PathBuf::from("extracted.mp4");
        let guard = crate::video::TempFileGuard::new(file.clone());

        // Fresh entry: the thumb job claims the in-flight slot.
        let _ = fire_archive_video_thumb(
            &app.shared.pipeline,
            &app.shared.thumbs,
            app.window.viewer_mut().unwrap(),
            entry.clone(),
            file.clone(),
            guard.clone(),
        );
        assert!(app.viewer().unwrap().in_flight_thumbs.contains(&entry));

        // Clear the slot, cache a thumb, and confirm a second fire skips.
        app.window
            .viewer_mut()
            .unwrap()
            .in_flight_thumbs
            .remove(&entry);
        cache_thumb(&mut app, "clip.mp4", 4, 2);
        let _ = fire_archive_video_thumb(
            &app.shared.pipeline,
            &app.shared.thumbs,
            app.window.viewer_mut().unwrap(),
            entry.clone(),
            file,
            guard,
        );
        assert!(!app.viewer().unwrap().in_flight_thumbs.contains(&entry));
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
