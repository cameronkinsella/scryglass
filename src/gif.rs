//! Animated GIF decoding, caching, and playback.
//!
//! GIF frames use disposal methods to minimize file size, so each frame may
//! only contain a small changed region rather than the full image. This
//! module preserves that compact representation: frames store only their
//! sub-rectangle pixels, position, disposal method, and delay.
//!
//! Compositing onto a full canvas happens at display time, one frame at a
//! time, keeping memory usage proportional to the GIF's actual data rather
//! than `width × height × frame_count`.
//!
//! The `GifPlayer` struct manages the decode cache, active animation state,
//! and GPU allocation lifecycle. It exposes an `update()` / message-driven
//! interface that `app.rs` wires into the iced application loop.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::Task;
use iced::widget::image::{Allocation, Handle};

use crate::cache;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A fully decoded GIF: all frames with their raw sub-rectangle data.
///
/// Cheap to hold in memory for pre-fetching: each frame stores only its
/// sub-rectangle pixels (not a full-canvas copy). Wrapped in `Arc` so it
/// can be shared between the prefetch cache and the active GIF state.
#[derive(Debug, Clone)]
pub struct DecodedGif {
    /// Full canvas dimensions.
    pub width: u32,
    pub height: u32,
    /// Decoded frames in order.
    pub frames: Vec<RawFrame>,
}

/// A single decoded frame, compact sub-rectangle representation.
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Sub-rectangle position within the canvas.
    pub left: u32,
    pub top: u32,
    /// Sub-rectangle dimensions.
    pub width: u32,
    pub height: u32,
    /// RGBA pixel data for the sub-rectangle only.
    pub pixels: Vec<u8>,
    /// How to dispose the canvas after displaying this frame.
    pub dispose: gif::DisposalMethod,
    /// Display duration for this frame.
    pub delay: Duration,
}

/// Error type for GIF decoding.
#[derive(Debug, thiserror::Error)]
pub enum GifError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("GIF decode error: {0}")]
    Decode(#[from] gif::DecodingError),
    #[error("GIF contains no frames")]
    NoFrames,
}

/// Messages produced and consumed by `GifPlayer`.
#[derive(Debug, Clone)]
pub enum GifMessage {
    /// GIF decode completed (off-thread).
    Decoded(PathBuf, Result<Arc<DecodedGif>, String>),
    /// A composited GIF frame was allocated to GPU memory.
    FrameAllocated(PathBuf, Result<Allocation, cache::Error>),
    /// Timer tick, advance to the next frame.
    Tick,
}

// ---------------------------------------------------------------------------
// Detection & decoding
// ---------------------------------------------------------------------------

/// Returns `true` if the file at `path` has a `.gif` extension.
pub fn is_gif(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// Decode all frames of a GIF into compact sub-rectangle representations.
///
/// Preserves the GIF's native compact format. Each `RawFrame` contains only
/// the pixels for its sub-rectangle region, plus disposal metadata.
pub fn decode_gif(path: &Path) -> Result<Arc<DecodedGif>, GifError> {
    use gif::DecodeOptions;
    use std::fs::File;

    let file = File::open(path)?;
    let mut opts = DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);

    let mut decoder = opts.read_info(file)?;
    let width = decoder.width() as u32;
    let height = decoder.height() as u32;

    let mut frames = Vec::new();

    while let Some(frame) = decoder.read_next_frame()? {
        let delay_cs = frame.delay.max(2) as u64;
        let delay = Duration::from_millis(delay_cs * 10);

        frames.push(RawFrame {
            left: frame.left as u32,
            top: frame.top as u32,
            width: frame.width as u32,
            height: frame.height as u32,
            pixels: frame.buffer.to_vec(),
            dispose: frame.dispose,
            delay,
        });
    }

    if frames.is_empty() {
        return Err(GifError::NoFrames);
    }

    Ok(Arc::new(DecodedGif {
        width,
        height,
        frames,
    }))
}

// ---------------------------------------------------------------------------
// Canvas compositor
// ---------------------------------------------------------------------------

/// Canvas for compositing GIF frames at display time.
///
/// Maintains a persistent pixel buffer and applies frames one at a time
/// according to their disposal methods.
pub struct GifCanvas {
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    prev_snapshot: Vec<u8>,
}

impl GifCanvas {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        Self {
            width,
            height,
            canvas: vec![0u8; size],
            prev_snapshot: vec![0u8; size],
        }
    }

    /// Composite the given frame onto the canvas.
    pub fn composite_frame(&mut self, frame: &RawFrame) {
        let cw = self.width as usize;
        let ch = self.height as usize;

        if matches!(frame.dispose, gif::DisposalMethod::Previous) {
            self.prev_snapshot.copy_from_slice(&self.canvas);
        }

        let fw = frame.width as usize;
        let fh = frame.height as usize;
        let fx = frame.left as usize;
        let fy = frame.top as usize;
        let buf = &frame.pixels;

        for row in 0..fh {
            let canvas_y = fy + row;
            if canvas_y >= ch {
                break;
            }
            for col in 0..fw {
                let canvas_x = fx + col;
                if canvas_x >= cw {
                    break;
                }
                let src_off = (row * fw + col) * 4;
                let dst_off = (canvas_y * cw + canvas_x) * 4;

                let alpha = buf[src_off + 3];
                if alpha == 255 {
                    self.canvas[dst_off..dst_off + 4].copy_from_slice(&buf[src_off..src_off + 4]);
                } else if alpha > 0 {
                    let sa = alpha as u32;
                    let da = 255 - sa;
                    for c in 0..3 {
                        let src = buf[src_off + c] as u32;
                        let dst = self.canvas[dst_off + c] as u32;
                        self.canvas[dst_off + c] = ((src * sa + dst * da) / 255) as u8;
                    }
                    self.canvas[dst_off + 3] =
                        (sa + (self.canvas[dst_off + 3] as u32 * da) / 255) as u8;
                }
            }
        }
    }

    pub fn pixels(&self) -> &[u8] {
        &self.canvas
    }

    /// Apply disposal method after the composited pixels have been consumed.
    pub fn apply_disposal(&mut self, frame: &RawFrame) {
        let cw = self.width as usize;
        let ch = self.height as usize;
        let fw = frame.width as usize;
        let fh = frame.height as usize;
        let fx = frame.left as usize;
        let fy = frame.top as usize;

        match frame.dispose {
            gif::DisposalMethod::Background => {
                for row in 0..fh {
                    let canvas_y = fy + row;
                    if canvas_y >= ch {
                        break;
                    }
                    for col in 0..fw {
                        let canvas_x = fx + col;
                        if canvas_x >= cw {
                            break;
                        }
                        let off = (canvas_y * cw + canvas_x) * 4;
                        self.canvas[off] = 0;
                        self.canvas[off + 1] = 0;
                        self.canvas[off + 2] = 0;
                        self.canvas[off + 3] = 0;
                    }
                }
            }
            gif::DisposalMethod::Previous => {
                self.canvas.copy_from_slice(&self.prev_snapshot);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// GifPlayer: owns cache, animation state, and drives the playback loop
// ---------------------------------------------------------------------------

/// Active animation state for the currently-displayed GIF.
struct ActiveGif {
    decoded: Arc<DecodedGif>,
    canvas: GifCanvas,
    frame_index: usize,
    /// Held to keep the current frame's GPU texture alive.
    _frame_allocation: Option<Allocation>,
}

/// Manages GIF decoding, caching, and animated playback.
///
/// The player owns the decode cache (`gif_cache`) and the active animation
/// state. It exposes a message-driven interface: call `update()` with a
/// `GifMessage`, get back a `Task<GifMessage>` and optional `Allocation` to
/// display.
pub struct GifPlayer {
    /// Pre-decoded GIF data, keyed by path.
    cache: HashMap<PathBuf, Arc<DecodedGif>>,
    /// Active animation (if viewing a GIF).
    active: Option<Box<ActiveGif>>,
}

impl GifPlayer {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            active: None,
        }
    }

    /// Handle a `GifMessage`, returning a task for follow-up work.
    ///
    /// `current_path` is the path the app is currently viewing. Used to
    /// discard stale messages.
    ///
    /// Returns `(task, allocation_ready)`:
    /// - `task`: async work to fire (frame allocation, etc.)
    /// - `allocation_ready`: if `Some`, the caller should display this allocation
    pub fn update(
        &mut self,
        msg: GifMessage,
        current_path: &Path,
    ) -> (Task<GifMessage>, Option<Allocation>) {
        match msg {
            GifMessage::Decoded(path, Ok(decoded)) => {
                self.cache.insert(path.clone(), Arc::clone(&decoded));

                if current_path == path {
                    let task = self.start_display(decoded, &path);
                    return (task, None);
                }
                (Task::none(), None)
            }

            GifMessage::Decoded(_path, Err(_err)) => (Task::none(), None),

            GifMessage::FrameAllocated(path, Ok(allocation)) => {
                if current_path != path {
                    return (Task::none(), None);
                }
                let Some(ag) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                ag._frame_allocation = Some(allocation.clone());
                (Task::none(), Some(allocation))
            }

            GifMessage::FrameAllocated(_path, Err(_err)) => (Task::none(), None),

            GifMessage::Tick => {
                let Some(ag) = self.active.as_mut() else {
                    return (Task::none(), None);
                };
                let frame_count = ag.decoded.frames.len();
                if frame_count <= 1 {
                    return (Task::none(), None);
                }

                // Apply disposal, advance, composite.
                let current_frame = &ag.decoded.frames[ag.frame_index];
                ag.canvas.apply_disposal(current_frame);
                ag.frame_index = (ag.frame_index + 1) % frame_count;
                let next_frame = &ag.decoded.frames[ag.frame_index];
                ag.canvas.composite_frame(next_frame);

                let pixels = ag.canvas.pixels().to_vec();
                let handle = Handle::from_rgba(ag.decoded.width, ag.decoded.height, pixels);
                let p = current_path.to_path_buf();
                let task = cache::allocate_handle(handle)
                    .map(move |result| GifMessage::FrameAllocated(p.clone(), result));
                (task, None)
            }
        }
    }

    /// Try to begin displaying a GIF at `path`.
    ///
    /// If the GIF is already in the decode cache, composites frame 0 and
    /// returns the allocation task immediately. Otherwise returns `None`, and
    /// the caller should fire a decode task.
    pub fn try_start_from_cache(&mut self, path: &Path) -> Option<Task<GifMessage>> {
        let decoded = self.cache.get(path)?.clone();
        Some(self.start_display(decoded, path))
    }

    /// Start displaying a decoded GIF: composite frame 0, fire allocation.
    fn start_display(&mut self, decoded: Arc<DecodedGif>, path: &Path) -> Task<GifMessage> {
        let mut canvas = GifCanvas::new(decoded.width, decoded.height);
        canvas.composite_frame(&decoded.frames[0]);

        let pixels = canvas.pixels().to_vec();
        let handle = Handle::from_rgba(decoded.width, decoded.height, pixels);

        self.active = Some(Box::new(ActiveGif {
            decoded,
            canvas,
            frame_index: 0,
            _frame_allocation: None,
        }));

        let p = path.to_path_buf();
        cache::allocate_handle(handle)
            .map(move |result| GifMessage::FrameAllocated(p.clone(), result))
    }

    /// Whether a decoded copy of `path` is in the cache, ready to display.
    pub fn has_cached(&self, path: &Path) -> bool {
        self.cache.contains_key(path)
    }

    /// Stop animation and drop the active GIF state.
    pub fn stop(&mut self) {
        self.active = None;
    }

    /// Whether a multi-frame GIF is currently active and ready to animate.
    pub fn is_animating(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|ag| ag.decoded.frames.len() > 1 && ag._frame_allocation.is_some())
    }

    /// The delay for the current frame (for the subscription timer).
    pub fn current_delay(&self) -> Option<Duration> {
        let ag = self.active.as_ref()?;
        if ag.decoded.frames.len() <= 1 {
            return None;
        }
        Some(ag.decoded.frames[ag.frame_index].delay)
    }

    /// Prune the decode cache to only keep paths in `keep`.
    pub fn prune_cache(&mut self, keep: &HashSet<PathBuf>) {
        self.cache.retain(|path, _| keep.contains(path));
    }

    /// Fire a decode task for `path` if it's a GIF and not already cached.
    /// Returns `Task::none()` if not a GIF or already cached.
    pub fn prefetch_decode(&self, path: &Path) -> Task<GifMessage> {
        if !is_gif(path) || self.cache.contains_key(path) {
            return Task::none();
        }
        let p = path.to_path_buf();
        Task::perform(
            async move {
                match decode_gif(&p) {
                    Ok(decoded) => (p, Ok(decoded)),
                    Err(e) => (p, Err(e.to_string())),
                }
            },
            |(path, result)| GifMessage::Decoded(path, result),
        )
    }

    /// Fire a decode task for the current GIF (not in cache).
    pub fn decode_current(&self, path: &Path) -> Task<GifMessage> {
        let p = path.to_path_buf();
        Task::perform(
            async move {
                match decode_gif(&p) {
                    Ok(decoded) => (p, Ok(decoded)),
                    Err(e) => (p, Err(e.to_string())),
                }
            },
            |(path, result)| GifMessage::Decoded(path, result),
        )
    }
}
