//! Animated image playback. Decoding lives in `media::animation` (and
//! arrives through the regular pipeline, so animations inside archives
//! play too). This module holds the per-window leases on the shared store's
//! decoded frames, the active playback state, and the GPU allocation lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::Task;
use iced::widget::image::Handle;

use crate::media::animation::{AnimatedImage, FrameCanvas};
use crate::media::store::{Anim, Lease};

/// Messages produced and consumed by `AnimPlayer`.
#[derive(Debug, Clone)]
pub enum AnimMessage {
    /// A composited frame finished uploading, with the keepalive holding its
    /// texture resident (or None if the upload could not run).
    FrameAllocated(PathBuf, Option<crate::ui::image_surface::Keepalive>),
    /// Timer tick, advance to the next frame.
    Tick,
}

/// Active playback state for the currently-displayed animation. The decoded frames
/// are NOT held here. They live in the shared store, read through this window's
/// lease (`cache[path]`) each tick, so playback falls back to the thumbnail only
/// once every window releases the GIF, and a backgrounded window keeps showing it
/// while another window holds it resident. Decay is shared across windows, like stills.
struct ActiveAnim {
    /// The animation's path, used to read the shared frames from this window's lease.
    path: PathBuf,
    canvas: FrameCanvas,
    frame_index: usize,
    /// The current frame's GPU texture, drawn directly by the view (which reads it
    /// via `current_texture`, like a still reads its store cell). `None` until the
    /// first frame uploads.
    frame_texture: Option<crate::ui::image_surface::Keepalive>,
}

/// Manages decoded-animation leases and playback.
pub struct AnimPlayer {
    /// Leases on decoded animations in the shared store, keyed by path. Holding a
    /// lease keeps the frames resident. The lease's `texture()` is the shared
    /// `Arc<AnimatedImage>`, so two windows on one GIF share a single decode and its
    /// decay, the same way two windows on one still share a texture.
    cache: HashMap<PathBuf, Lease<Anim>>,
    /// Active playback (if viewing an animation).
    active: Option<Box<ActiveAnim>>,
}

impl AnimPlayer {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            active: None,
        }
    }

    /// Hold a lease on a decoded animation in the shared store. The update layer
    /// mints the lease (sharing another window's decode when one exists) and hands
    /// it here. Dropping it later releases this window's claim on the frames.
    pub fn insert(&mut self, path: PathBuf, lease: Lease<Anim>) {
        self.cache.insert(path, lease);
    }

    /// Handle an `AnimMessage`. Messages stale against `current_path` are
    /// discarded. Returns follow-up work and an allocation to display, if any.
    pub fn update(
        &mut self,
        msg: AnimMessage,
        current_path: &Path,
    ) -> (Task<AnimMessage>, Option<(u32, u32)>) {
        match msg {
            AnimMessage::FrameAllocated(path, Some(keepalive)) => {
                if current_path != path {
                    return (Task::none(), None);
                }
                let Some(frames) = self.active_frames() else {
                    return (Task::none(), None);
                };
                let Some(active) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                // The frame's texture is resident (uploaded off-thread). Store it;
                // the view reads it via `current_texture`. Return the dimensions so
                // the caller switches the display to this animation on the first
                // frame and re-renders on each subsequent one.
                active.frame_texture = Some(keepalive);
                (Task::none(), Some((frames.width, frames.height)))
            }

            AnimMessage::FrameAllocated(_path, None) => (Task::none(), None),

            AnimMessage::Tick => {
                // Read the shared frames through this window's lease. If they were
                // evicted (every window released the GIF), keep the active state
                // dormant rather than dropping it: it holds its frame position so it
                // resumes from here the instant any window brings the frames back.
                let Some(frames) = self.active_frames() else {
                    return (Task::none(), None);
                };
                let Some(active) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                let frame_count = frames.frames.len();
                if frame_count <= 1 {
                    return (Task::none(), None);
                }
                // A re-decode after eviction can come back shorter (the file was
                // replaced on disk), leaving the dormant index past the end.
                if active.frame_index >= frame_count {
                    active.frame_index = 0;
                }

                // Apply disposal, advance, composite.
                let current_frame = &frames.frames[active.frame_index];
                active.canvas.apply_disposal(current_frame);
                active.frame_index = (active.frame_index + 1) % frame_count;
                let next_frame = &frames.frames[active.frame_index];
                active.canvas.composite_frame(next_frame);

                let pixels = active.canvas.pixels().to_vec();
                let handle = Handle::from_rgba(frames.width, frames.height, pixels);
                // Reuse the animation's existing texture by writing the new frame
                // into it (its size never changes frame to frame), sparing a
                // per-frame allocate and free. The texture is what is reused, not
                // the pixel snapshot: the canvas keeps compositing the next frame,
                // so the to_vec snapshot decouples it from the in-flight write.
                let dims = (frames.width, frames.height);
                let existing = active.frame_texture.as_ref().and_then(|k| k.size());
                let task = if should_reuse(existing, dims) {
                    let into = active.frame_texture.clone().expect("reuse checked size");
                    write_frame(handle, into, current_path.to_path_buf())
                } else {
                    // First frame, or a re-decode that changed dimensions.
                    upload_frame(handle, current_path.to_path_buf())
                };
                (task, None)
            }
        }
    }

    /// The shared frames backing the active animation, read through this window's
    /// lease. `None` if nothing is active or the frames were evicted (every window
    /// released the GIF), which is the signal for playback to fall back to the thumb.
    fn active_frames(&self) -> Option<Arc<AnimatedImage>> {
        let active = self.active.as_ref()?;
        self.cache.get(&active.path)?.texture()
    }

    /// This window's lease on `path`, if held, for re-asserting or lowering its
    /// demand on the shared frames during restore and decay.
    pub fn lease(&self, path: &Path) -> Option<&Lease<Anim>> {
        self.cache.get(path)
    }

    /// Whether playback is active on `path` (running or dormant after eviction), so a
    /// restore re-pins the frames and lets it resume from where it is, rather than
    /// restarting it from the first frame.
    pub fn is_active_on(&self, path: &Path) -> bool {
        self.active.as_ref().is_some_and(|a| a.path == path)
    }

    /// Begin playback if this window holds a lease whose frames are resident
    /// (composites frame 0). `None` means the caller should lease or decode it.
    pub fn try_start_from_cache(&mut self, path: &Path) -> Option<Task<AnimMessage>> {
        let decoded = self.cache.get(path)?.texture()?;
        Some(self.start_display(decoded, path))
    }

    /// Start displaying a decoded animation: composite frame 0, fire its GPU
    /// allocation. `frames` come from this window's lease. The active state keeps
    /// only the canvas and index and re-reads the frames from the lease each tick.
    fn start_display(&mut self, frames: Arc<AnimatedImage>, path: &Path) -> Task<AnimMessage> {
        // A decoder can hand back an animation with no frames. Show nothing
        // rather than indexing into an empty list.
        let Some(first) = frames.frames.first() else {
            return Task::none();
        };
        let mut canvas = FrameCanvas::new(frames.width, frames.height);
        canvas.composite_frame(first);

        let pixels = canvas.pixels().to_vec();
        let handle = Handle::from_rgba(frames.width, frames.height, pixels);

        self.active = Some(Box::new(ActiveAnim {
            path: path.to_path_buf(),
            canvas,
            frame_index: 0,
            frame_texture: None,
        }));

        upload_frame(handle, path.to_path_buf())
    }

    /// Whether this window holds a lease on `path` whose frames are resident, ready
    /// to display with no decode.
    pub fn has_cached(&self, path: &Path) -> bool {
        self.cache
            .get(path)
            .is_some_and(|lease| lease.texture().is_some())
    }

    /// Stop playback and drop the active state.
    pub fn stop(&mut self) {
        self.active = None;
    }

    /// The current frame's resident texture, read directly by the view. `None`
    /// while the first frame uploads, or once the shared frames are evicted
    /// (every window released the GIF), so the view falls back to the thumbnail
    /// in lockstep with the shared decay state, not on this window's own timer.
    pub fn current_texture(&self) -> Option<crate::ui::image_surface::Keepalive> {
        self.active_frames()?;
        self.active.as_ref()?.frame_texture.clone()
    }

    /// The current frame's pixels as a handle, for clipboard copy.
    pub fn current_handle(&self) -> Option<Handle> {
        let frames = self.active_frames()?;
        let active = self.active.as_ref()?;
        Some(Handle::from_rgba(
            frames.width,
            frames.height,
            active.canvas.pixels().to_vec(),
        ))
    }

    /// Whether a multi-frame animation is active, resident, and ready to animate.
    pub fn is_animating(&self) -> bool {
        let Some(frames) = self.active_frames() else {
            return false;
        };
        self.active
            .as_ref()
            .is_some_and(|a| frames.frames.len() > 1 && a.frame_texture.is_some())
    }

    /// The delay for the current frame (for the subscription timer).
    pub fn current_delay(&self) -> Option<Duration> {
        let frames = self.active_frames()?;
        let active = self.active.as_ref()?;
        if frames.frames.len() <= 1 {
            return None;
        }
        Some(frames.frames[active.frame_index].delay)
    }

    /// Drop the leases for paths outside `keep`. The shared store frees a GIF's
    /// frames once its last window's lease goes, so this is the look-ahead pruning.
    pub fn prune_cache(&mut self, keep: &HashSet<PathBuf>) {
        self.cache.retain(|path, _| keep.contains(path));
    }

    /// Drop a single lease (file deleted or renamed).
    pub fn remove(&mut self, path: &Path) {
        self.cache.remove(path);
    }
}

/// Whether the next frame can be written into the existing texture instead of
/// allocating a fresh one: a texture already exists and its size matches the
/// animation's dimensions. A re-decode that changed size needs a fresh upload.
fn should_reuse(existing_size: Option<(u32, u32)>, dims: (u32, u32)) -> bool {
    existing_size == Some(dims)
}

/// Upload a composited frame through the still-image worker (off the render
/// thread) and report its keepalive token. Retries briefly so a frame composited
/// before the first render still finds the upload worker.
fn upload_frame(handle: Handle, path: PathBuf) -> Task<AnimMessage> {
    Task::future(async move {
        for _ in 0..50 {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            if crate::ui::image_surface::submit_upload(handle.clone(), ready_tx) {
                return AnimMessage::FrameAllocated(path, ready_rx.await.ok());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        AnimMessage::FrameAllocated(path, None)
    })
}

/// Write a composited frame into the animation's existing texture in place (no
/// allocation) and report the same keepalive back. Modeled on `upload_frame`,
/// retrying briefly for the same reason. `FrameAllocated` then re-stores the
/// returned keepalive and drives the redraw exactly as a fresh upload does.
fn write_frame(
    handle: Handle,
    into: crate::ui::image_surface::Keepalive,
    path: PathBuf,
) -> Task<AnimMessage> {
    Task::future(async move {
        for _ in 0..50 {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            if crate::ui::image_surface::submit_write_frame(handle.clone(), into.clone(), ready_tx)
            {
                return AnimMessage::FrameAllocated(path, ready_rx.await.ok());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        AnimMessage::FrameAllocated(path, None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::animation::RawFrame;
    use crate::media::pipeline::Source;
    use crate::media::store::{AnimRam, ImageKey, Store, Tier};

    /// A tiny `frames`-frame 2x2 animation for exercising the player without GPU.
    fn anim(frames: usize) -> Arc<AnimatedImage> {
        let frame = || RawFrame {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            pixels: vec![0u8; 16],
            dispose: gif::DisposalMethod::Keep,
            delay: Duration::from_millis(100),
        };
        Arc::new(AnimatedImage {
            width: 2,
            height: 2,
            frames: (0..frames).map(|_| frame()).collect(),
            thumbnail: None,
        })
    }

    /// Mint a resident lease for `path` in the shared store, as the update layer
    /// does after a decode lands. The store can be dropped right after: the lease
    /// keeps its shared cell alive on its own, so the frames stay readable.
    fn lease_for(store: &mut Store<Anim>, path: &Path, frames: usize) -> Lease<Anim> {
        let key = ImageKey::new(&Source::Fs, path);
        let (lease, _) = store.request(key.clone(), path.to_path_buf(), Source::Fs, Tier::InRam);
        store.on_decoded(
            key,
            AnimRam {
                frames: anim(frames),
                decode_time: None,
            },
        );
        lease
    }

    // The Tick reuse-vs-upload choice depends only on the existing texture's
    // size, which needs a real device to produce. So the decision is factored
    // into the pure `should_reuse` and tested here directly: after the first
    // frame lands with a size, a same-dimension tick takes the write-into path,
    // while a missing or size-changed texture falls back to a fresh upload.
    #[test]
    fn reuse_decision_matches_on_dimensions() {
        // First frame: no texture yet, so allocate fresh.
        assert!(!should_reuse(None, (2, 2)));
        // Same dimensions as the resident texture: write into it.
        assert!(should_reuse(Some((2, 2)), (2, 2)));
        // A re-decode changed size: allocate fresh instead of writing.
        assert!(!should_reuse(Some((3, 3)), (2, 2)));
        assert!(!should_reuse(Some((2, 3)), (2, 2)));
    }

    #[test]
    fn empty_player_derives_nothing() {
        let player = AnimPlayer::new();
        assert!(player.current_texture().is_none());
        assert!(player.current_handle().is_none());
        assert!(!player.is_animating());
        assert!(player.current_delay().is_none());
    }

    #[test]
    fn starting_from_cache_is_active_with_texture_pending() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let path = Path::new("a.gif");
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 3));
        assert!(player.has_cached(path));

        // Compositing frame 0 makes the animation active: a handle is derivable
        // from the canvas and the delay drives the timer, but the texture is None
        // until the async upload lands (so the view shows the thumbnail meanwhile).
        let _task = player.try_start_from_cache(path);
        assert!(player.current_handle().is_some());
        assert!(player.current_texture().is_none());
        assert!(player.current_delay().is_some());
        // The tick timer is gated on the first frame being resident, so it does
        // not run until a texture has landed.
        assert!(!player.is_animating());
    }

    #[test]
    fn evicting_the_active_animation_frees_it() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let path = Path::new("a.gif");
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 3));
        let _task = player.try_start_from_cache(path);
        assert!(player.current_handle().is_some());
        assert!(player.has_cached(path));

        // Decay lowers this window's demand to evicted. As the only holder, the
        // shared frames free, so playback derives nothing and the view shows the
        // thumbnail (the player follows the shared decay state, not its own timer).
        store.retarget(player.lease(path).unwrap(), Tier::Evicted);
        assert!(player.current_handle().is_none());
        assert!(player.current_texture().is_none());
        assert!(!player.has_cached(path));
    }

    #[test]
    fn evicting_another_path_keeps_the_active_one() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let active = Path::new("a.gif");
        let other = Path::new("b.gif");
        player.insert(active.to_path_buf(), lease_for(&mut store, active, 2));
        player.insert(other.to_path_buf(), lease_for(&mut store, other, 2));
        let _task = player.try_start_from_cache(active);

        // Evicting the other path frees only its frames. The active one is untouched.
        store.retarget(player.lease(other).unwrap(), Tier::Evicted);
        assert!(player.current_handle().is_some());
        assert!(player.has_cached(active));
        assert!(!player.has_cached(other));
    }

    #[test]
    fn a_backgrounded_window_keeps_showing_a_gif_another_window_holds() {
        // Two windows share one GIF. One lowers its demand to evicted (it
        // backgrounds), but the frames stay resident because the other still holds
        // them, so the first window keeps deriving its display without blanking.
        let mut store = Store::<Anim>::default();
        let path = Path::new("a.gif");
        let key = ImageKey::new(&Source::Fs, path);

        let mut backgrounded = AnimPlayer::new();
        let (held, _) = store.request(key.clone(), path.to_path_buf(), Source::Fs, Tier::InRam);
        store.on_decoded(
            key.clone(),
            AnimRam {
                frames: anim(3),
                decode_time: None,
            },
        );
        // The backgrounded window leases the same resident entry (no second decode).
        let (bg_lease, _) = store.request(key.clone(), path.to_path_buf(), Source::Fs, Tier::InRam);
        backgrounded.insert(path.to_path_buf(), bg_lease);
        let _task = backgrounded.try_start_from_cache(path);
        assert!(backgrounded.current_handle().is_some());

        // The backgrounded window drops to evicted demand, but `held` keeps the GIF
        // resident, so its display survives.
        store.retarget(backgrounded.lease(path).unwrap(), Tier::Evicted);
        assert!(backgrounded.current_handle().is_some());
        assert!(backgrounded.has_cached(path));

        // The other window releases it too: now the frames free and the backgrounded
        // window finally derives nothing.
        drop(held);
        let _ = store.pump();
        assert!(backgrounded.current_handle().is_none());
        assert!(!backgrounded.has_cached(path));
    }

    #[test]
    fn a_dormant_window_resumes_when_the_gif_returns() {
        // After a shared eviction the playback is kept (dormant), so when any window
        // brings the frames back this window derives its display again, resuming from
        // where it was rather than needing a fresh start.
        let mut store = Store::<Anim>::default();
        let path = Path::new("a.gif");
        let key = ImageKey::new(&Source::Fs, path);

        let mut player = AnimPlayer::new();
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 3));
        let _ = player.try_start_from_cache(path);
        assert!(player.current_handle().is_some());
        assert!(player.is_active_on(path));

        // The last holder evicts: the frames free, so the display derives nothing, but
        // the playback stays active on the path (dormant), not torn down.
        store.retarget(player.lease(path).unwrap(), Tier::Evicted);
        assert!(player.current_handle().is_none());
        assert!(player.is_active_on(path));

        // The GIF is decoded again (re-pinned): the kept playback derives its display
        // once more, with no fresh start.
        store.retarget(player.lease(path).unwrap(), Tier::InRam);
        store.on_decoded(
            key,
            AnimRam {
                frames: anim(3),
                decode_time: None,
            },
        );
        assert!(player.current_handle().is_some());
        assert!(player.is_active_on(path));
    }

    #[test]
    fn stopping_derives_nothing_again() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let path = Path::new("a.gif");
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 2));
        let _task = player.try_start_from_cache(path);
        player.stop();
        assert!(player.current_handle().is_none());
        assert!(player.current_texture().is_none());
        // The decode stays cached for an instant restart.
        assert!(player.has_cached(path));
    }

    #[test]
    fn a_shorter_redecode_resets_the_dormant_frame_index() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let path = Path::new("a.gif");
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 3));
        let _ = player.try_start_from_cache(path);
        // Advance to the last frame, then go dormant through an eviction.
        let _ = player.update(AnimMessage::Tick, path);
        let _ = player.update(AnimMessage::Tick, path);
        store.retarget(player.lease(path).unwrap(), Tier::Evicted);

        // The file was replaced on disk, so the re-decode comes back shorter.
        store.retarget(player.lease(path).unwrap(), Tier::InRam);
        store.on_decoded(
            ImageKey::new(&Source::Fs, path),
            AnimRam {
                frames: anim(2),
                decode_time: None,
            },
        );

        // The dormant index sits past the new end. The tick resets it instead
        // of indexing out of bounds.
        let _ = player.update(AnimMessage::Tick, path);
        assert!(player.is_active_on(path));
    }

    #[test]
    fn an_empty_decode_starts_nothing_instead_of_panicking() {
        let mut store = Store::<Anim>::default();
        let mut player = AnimPlayer::new();
        let path = Path::new("a.gif");
        player.insert(path.to_path_buf(), lease_for(&mut store, path, 0));
        assert!(player.try_start_from_cache(path).is_some());
        assert!(!player.is_active_on(path));
        assert!(player.current_texture().is_none());
    }
}
