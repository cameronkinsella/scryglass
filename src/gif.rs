//! Animated GIF decoding with compact frame storage.
//!
//! GIF frames use disposal methods to minimize file size, so each frame may
//! only contain a small changed region rather than the full image. This
//! module preserves that compact representation: frames store only their
//! sub-rectangle pixels, position, disposal method, and delay.
//!
//! Compositing onto a full canvas happens at display time, one frame at a
//! time, keeping memory usage proportional to the GIF's actual data rather
//! than `width × height × frame_count`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A fully decoded GIF: all frames with their raw sub-rectangle data.
///
/// This is cheap to hold in memory for pre-fetching: each frame stores only
/// its sub-rectangle pixels (not a full-canvas copy). Wrapped in `Arc` so
/// it can be shared between the prefetch cache and the active GIF state.
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
    /// Length = `width * height * 4`.
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

/// Returns `true` if the file at `path` has a `.gif` extension.
pub fn is_gif(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// Decode all frames of a GIF into compact sub-rectangle representations.
///
/// This does NOT composite frames onto a canvas. It preserves the GIF's
/// native compact format. Each `RawFrame` contains only the pixels for its
/// sub-rectangle region, plus disposal metadata for compositing at display time.
///
/// Returns an `Arc<DecodedGif>` suitable for caching.
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
        // GIF delay is in centiseconds, minimum 20ms to avoid zero-delay spinning.
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

/// Canvas for compositing GIF frames at display time.
///
/// Maintains a persistent pixel buffer and applies frames one at a time
/// according to their disposal methods. Only one full-canvas buffer exists
/// at any time (plus a snapshot buffer for `Previous` disposal).
pub struct GifCanvas {
    width: u32,
    height: u32,
    /// Current composited canvas (RGBA, `width * height * 4` bytes).
    canvas: Vec<u8>,
    /// Snapshot for `DisposalMethod::Previous`.
    prev_snapshot: Vec<u8>,
}

impl GifCanvas {
    /// Create a new canvas for the given GIF dimensions.
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
    ///
    /// After this call, the canvas contains the fully composited image for
    /// this frame. Call `pixels()` to read the result, then `apply_disposal()`
    /// to prepare for the next frame.
    pub fn composite_frame(&mut self, frame: &RawFrame) {
        let cw = self.width as usize;
        let ch = self.height as usize;

        // Save canvas before compositing if Previous disposal needed.
        if matches!(frame.dispose, gif::DisposalMethod::Previous) {
            self.prev_snapshot.copy_from_slice(&self.canvas);
        }

        // Composite frame pixels onto canvas.
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

    /// Return the current canvas pixels.
    pub fn pixels(&self) -> &[u8] {
        &self.canvas
    }

    /// Apply disposal method after the caller has consumed the composited pixels.
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

    /// Reset the canvas to transparent.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.canvas.fill(0);
        self.prev_snapshot.fill(0);
    }

    /// Composite frames from 0 to `target_index` (inclusive) and return
    /// a clone of the resulting canvas pixels.
    ///
    /// Used when seeking to a specific frame (e.g., displaying the first
    /// frame of a pre-fetched GIF).
    #[allow(dead_code)]
    pub fn composite_up_to(&mut self, gif: &DecodedGif, target_index: usize) -> Vec<u8> {
        self.clear();
        for (i, frame) in gif.frames.iter().enumerate() {
            self.composite_frame(frame);
            if i == target_index {
                let result = self.canvas.clone();
                self.apply_disposal(frame);
                return result;
            }
            self.apply_disposal(frame);
        }
        self.canvas.clone()
    }
}
