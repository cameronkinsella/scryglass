//! Tile pyramid demand and production for stills past the texture limit.

use iced::Task;
use iced::widget::image::Handle;

use crate::app::viewer_math::compute_zoom;
use crate::app::{MediaMessage, Message, Shared, Window};
use crate::media::store::{ImageKey, RamImage};

use super::submit_and_wait;

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
        compute_zoom(
            shared.config.standard.display.zoom_mode,
            true_size.0,
            true_size.1,
            viewport,
        )
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
    let Some((_, src)) = crate::ui::geometry::display_geometry(
        zoom,
        viewer.pan,
        (viewport.width, viewport.height),
        true_size,
    ) else {
        return Task::none();
    };
    if displayed.0 <= max && displayed.1 <= max {
        // Demand targets the size the draw actually spanned, so the drawn
        // grid and the produced tiles can never disagree by a rounding
        // step. Before the first tiled draw, the settle that follows the
        // mint brings demand back here.
        let target = set.draw_shown();
        if target == (0, 0) {
            return Task::none();
        }
        // A base already at the shown size is the exact copy by itself.
        if set.base().and_then(|base| base.size()) == Some(target) {
            return Task::none();
        }
        set.ensure_exact(target);
        // Visible exact tiles plus half a tile of pan margin, capped at
        // what one frame can draw (an 8K-plus viewport degrades to the
        // base past the cap instead of churning).
        let margin = (
            tiles::TILE_SIZE as f32 / (2.0 * target.0 as f32),
            tiles::TILE_SIZE as f32 / (2.0 * target.1 as f32),
        );
        let wanted = [
            src[0] - margin.0,
            src[1] - margin.1,
            src[2] + margin.0,
            src[3] + margin.1,
        ];
        let jobs: Vec<Task<Message>> = tiles::window_tiles(wanted, target)
            .map(|(col, row)| tiles::TileKey { lod: 0, col, row })
            .take(crate::ui::image_surface::MAX_TILE_DRAWS)
            .filter(|tile| set.try_claim_exact(target, *tile))
            .map(|tile| produce_exact(key.clone(), &ram.handle, resident.clone(), target, tile))
            .collect();
        return Task::batch(jobs);
    }
    // Past one texture's worth of display, tiles are the only way.
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

/// Produce one exact-scale tile: a byte-exact crop of the one-pass
/// downscale at `target`, cut for the visible region only.
fn produce_exact(
    key: ImageKey,
    handle: &Handle,
    pyramid: crate::ui::image_surface::Keepalive,
    target: (u32, u32),
    tile: crate::media::tiles::TileKey,
) -> Task<Message> {
    let handle = handle.clone();
    Task::future(async move {
        let _permit = TILE_GATE.acquire().await.ok();
        use crate::media::tiles;
        // Bail when the rest moved to another size while this sat queued.
        let stale = pyramid
            .tiles()
            .is_none_or(|set| set.exact_target() != target);
        let produced = if stale {
            None
        } else {
            let kernel = crate::ui::image_surface::current_kernel();
            tokio::task::spawn_blocking(move || {
                let Handle::Rgba {
                    width,
                    height,
                    pixels,
                    ..
                } = &handle
                else {
                    return None;
                };
                let rect = tiles::tile_rect(target, tile.col, tile.row);
                let cut = crate::media::resample::downscale_window(
                    pixels.as_ref(),
                    (*width, *height),
                    target,
                    rect,
                    kernel,
                );
                Some(Handle::from_rgba(rect.2, rect.3, cut))
            })
            .await
            .ok()
            .flatten()
        };
        let texture = match produced {
            Some(cut) => submit_and_wait(cut).await,
            None => None,
        };
        Message::Media(MediaMessage::ExactReady {
            key,
            target,
            tile,
            texture,
            pyramid,
        })
    })
}

/// Produce and upload one tile of a tiled still. The tile is cut from the RAM
/// substrate with the region resampler, whose border taps read the whole
/// image, so adjacent tiles reassemble seamlessly. The finished tile uploads
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
