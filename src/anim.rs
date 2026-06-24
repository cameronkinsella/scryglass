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

use crate::allocation;
use crate::allocation::Allocation;
use crate::media::animation::{AnimatedImage, FrameCanvas};

/// Messages produced and consumed by `AnimPlayer`.
#[derive(Debug, Clone)]
pub enum AnimMessage {
    /// A composited frame was allocated to GPU memory.
    FrameAllocated(PathBuf, Result<Allocation, allocation::Error>),
    /// Timer tick, advance to the next frame.
    Tick,
}

/// Active playback state for the currently-displayed animation.
struct ActiveAnim {
    decoded: Arc<AnimatedImage>,
    canvas: FrameCanvas,
    frame_index: usize,
    /// Held to keep the current frame's GPU texture alive.
    _frame_allocation: Option<Allocation>,
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
    ) -> (Task<AnimMessage>, Option<Allocation>) {
        match msg {
            AnimMessage::FrameAllocated(path, Ok(allocation)) => {
                if current_path != path {
                    return (Task::none(), None);
                }
                let Some(active) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                active._frame_allocation = Some(allocation.clone());
                (Task::none(), Some(allocation))
            }

            AnimMessage::FrameAllocated(_path, Err(_err)) => (Task::none(), None),

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
                let p = current_path.to_path_buf();
                let task = allocation::allocate_handle(handle)
                    .map(move |result| AnimMessage::FrameAllocated(p.clone(), result));
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
            _frame_allocation: None,
        }));

        let p = path.to_path_buf();
        allocation::allocate_handle(handle)
            .map(move |result| AnimMessage::FrameAllocated(p.clone(), result))
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
            .is_some_and(|a| a.decoded.frames.len() > 1 && a._frame_allocation.is_some())
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
