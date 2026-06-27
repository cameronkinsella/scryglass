//! Animated image playback. Decoding lives in `media::animation` (and
//! arrives through the regular pipeline, so animations inside archives
//! play too). This module owns the decoded-animation cache, the active
//! playback state, and the GPU allocation lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::Task;
use iced::widget::image::Handle;

use crate::media::animation::{AnimatedImage, FrameCanvas};

/// Messages produced and consumed by `AnimPlayer`.
#[derive(Debug, Clone)]
pub enum AnimMessage {
    /// A composited frame finished uploading: its handle plus the keepalive
    /// holding its texture resident, or None if the upload could not run.
    FrameAllocated(
        PathBuf,
        Option<(Handle, crate::ui::image_surface::Keepalive)>,
    ),
    /// Timer tick, advance to the next frame.
    Tick,
}

/// Active playback state for the currently-displayed animation.
struct ActiveAnim {
    decoded: Arc<AnimatedImage>,
    canvas: FrameCanvas,
    frame_index: usize,
    /// Held to keep the current frame's GPU texture alive.
    _frame_keepalive: Option<crate::ui::image_surface::Keepalive>,
}

/// Manages decoded-animation caching and playback.
pub struct AnimPlayer {
    /// Decoded animations, keyed by path, fed by pipeline loads.
    cache: HashMap<PathBuf, Arc<AnimatedImage>>,
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

    /// Store a decoded animation (from a pipeline load or prefetch).
    pub fn insert(&mut self, path: PathBuf, anim: Arc<AnimatedImage>) {
        self.cache.insert(path, anim);
    }

    /// Handle an `AnimMessage`. Messages stale against `current_path` are
    /// discarded. Returns follow-up work and an allocation to display, if any.
    pub fn update(
        &mut self,
        msg: AnimMessage,
        current_path: &Path,
    ) -> (Task<AnimMessage>, Option<Handle>) {
        match msg {
            AnimMessage::FrameAllocated(path, Some((handle, keepalive))) => {
                if current_path != path {
                    return (Task::none(), None);
                }
                let Some(active) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                // The keepalive holds the frame's texture resident, already
                // uploaded off-thread, so it is ready to draw.
                active._frame_keepalive = Some(keepalive);
                (Task::none(), Some(handle))
            }

            AnimMessage::FrameAllocated(_path, None) => (Task::none(), None),

            AnimMessage::Tick => {
                let Some(active) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                let frame_count = active.decoded.frames.len();
                if frame_count <= 1 {
                    return (Task::none(), None);
                }

                // Apply disposal, advance, composite.
                let current_frame = &active.decoded.frames[active.frame_index];
                active.canvas.apply_disposal(current_frame);
                active.frame_index = (active.frame_index + 1) % frame_count;
                let next_frame = &active.decoded.frames[active.frame_index];
                active.canvas.composite_frame(next_frame);

                let pixels = active.canvas.pixels().to_vec();
                let handle = Handle::from_rgba(active.decoded.width, active.decoded.height, pixels);
                let task = upload_frame(handle, current_path.to_path_buf());
                (task, None)
            }
        }
    }

    /// Begin playback if `path`'s decode is cached (composites frame 0).
    /// `None` means the caller should fire a pipeline load.
    pub fn try_start_from_cache(&mut self, path: &Path) -> Option<Task<AnimMessage>> {
        let decoded = self.cache.get(path)?.clone();
        Some(self.start_display(decoded, path))
    }

    /// Start displaying a decoded animation: composite frame 0, fire its
    /// GPU allocation.
    fn start_display(&mut self, decoded: Arc<AnimatedImage>, path: &Path) -> Task<AnimMessage> {
        let mut canvas = FrameCanvas::new(decoded.width, decoded.height);
        canvas.composite_frame(&decoded.frames[0]);

        let pixels = canvas.pixels().to_vec();
        let handle = Handle::from_rgba(decoded.width, decoded.height, pixels);

        self.active = Some(Box::new(ActiveAnim {
            decoded,
            canvas,
            frame_index: 0,
            _frame_keepalive: None,
        }));

        upload_frame(handle, path.to_path_buf())
    }

    /// Whether a decoded copy of `path` is cached, ready to display.
    pub fn has_cached(&self, path: &Path) -> bool {
        self.cache.contains_key(path)
    }

    /// Stop playback and drop the active state.
    pub fn stop(&mut self) {
        self.active = None;
    }

    /// Whether a multi-frame animation is active and ready to animate.
    pub fn is_animating(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.decoded.frames.len() > 1 && a._frame_keepalive.is_some())
    }

    /// The delay for the current frame (for the subscription timer).
    pub fn current_delay(&self) -> Option<Duration> {
        let active = self.active.as_ref()?;
        if active.decoded.frames.len() <= 1 {
            return None;
        }
        Some(active.decoded.frames[active.frame_index].delay)
    }

    /// Prune the decode cache to only keep paths in `keep`.
    pub fn prune_cache(&mut self, keep: &HashSet<PathBuf>) {
        self.cache.retain(|path, _| keep.contains(path));
    }

    /// Drop a single cached decode (file deleted or renamed).
    pub fn remove(&mut self, path: &Path) {
        self.cache.remove(path);
    }
}

/// Upload a composited frame through the still-image worker (off the render
/// thread) and report its keepalive token. Retries briefly so a frame composited
/// before the first render still finds the upload worker.
fn upload_frame(handle: Handle, path: PathBuf) -> Task<AnimMessage> {
    Task::future(async move {
        for _ in 0..50 {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            if crate::ui::image_surface::submit_upload(handle.clone(), ready_tx) {
                let keepalive = ready_rx.await.ok();
                return AnimMessage::FrameAllocated(path, keepalive.map(|k| (handle, k)));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        AnimMessage::FrameAllocated(path, None)
    })
}
