//! Animated image data and frame compositing, shared by the decoders
//! (which produce [`AnimatedImage`]) and the app-side player (which
//! composites frames at display time).
//!
//! GIF frames use disposal methods to minimize file size, so each frame may
//! only contain a small changed region rather than the full image. The
//! compact sub-rectangle representation is preserved here. Compositing
//! onto a full canvas happens one frame at a time, keeping memory
//! proportional to the file's actual data rather than
//! `width × height × frame_count`. APNG and animated WebP arrive from the
//! `image` crate as full-canvas frames and use the same representation
//! with a full-size rect.

use std::time::Duration;

use super::{MediaError, THUMB_DIM, ThumbData};

/// A fully decoded animation: all frames with their raw sub-rect data.
#[derive(Debug, Clone)]
pub struct AnimatedImage {
    /// Full canvas dimensions.
    pub width: u32,
    pub height: u32,
    /// Decoded frames in order.
    pub frames: Vec<RawFrame>,
    /// Thumbnail derived from the first frame at decode time.
    pub thumbnail: Option<ThumbData>,
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

/// Decode a GIF from bytes into compact sub-rectangle frames.
pub fn decode_gif_bytes(bytes: &[u8]) -> Result<AnimatedImage, MediaError> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);

    let mut decoder = opts
        .read_info(std::io::Cursor::new(bytes))
        .map_err(|e| MediaError::Decode(e.to_string()))?;
    let width = decoder.width() as u32;
    let height = decoder.height() as u32;

    let mut frames = Vec::new();
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|e| MediaError::Decode(e.to_string()))?
    {
        let delay_cs = frame.delay.max(2) as u64;
        frames.push(RawFrame {
            left: frame.left as u32,
            top: frame.top as u32,
            width: frame.width as u32,
            height: frame.height as u32,
            pixels: frame.buffer.to_vec(),
            dispose: frame.dispose,
            delay: Duration::from_millis(delay_cs * 10),
        });
    }
    if frames.is_empty() {
        return Err(MediaError::Decode("animation contains no frames".into()));
    }

    Ok(finish_animation(width, height, frames))
}

/// Collect frames from an `image` crate animation decoder (APNG, animated
/// WebP). These arrive as full-canvas composited frames, so `Background`
/// disposal makes each frame replace the previous one.
pub fn from_image_frames<'a>(
    decoder: impl image::AnimationDecoder<'a>,
) -> Result<AnimatedImage, MediaError> {
    let mut frames = Vec::new();
    let (mut width, mut height) = (0, 0);
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| MediaError::Decode(e.to_string()))?;
        let delay = Duration::from(frame.delay());
        let buffer = frame.into_buffer();
        let (w, h) = buffer.dimensions();
        width = width.max(w);
        height = height.max(h);
        frames.push(RawFrame {
            left: 0,
            top: 0,
            width: w,
            height: h,
            pixels: buffer.into_raw(),
            dispose: gif::DisposalMethod::Background,
            // Zero-delay frames stall playback, clamp like GIF's 2cs floor.
            delay: delay.max(Duration::from_millis(20)),
        });
    }
    if frames.is_empty() {
        return Err(MediaError::Decode("animation contains no frames".into()));
    }
    Ok(finish_animation(width, height, frames))
}

/// Derive the first-frame thumbnail and assemble the result.
fn finish_animation(width: u32, height: u32, frames: Vec<RawFrame>) -> AnimatedImage {
    let mut canvas = FrameCanvas::new(width, height);
    canvas.composite_frame(&frames[0]);
    let first = canvas.pixels().to_vec();

    let thumbnail = if width.max(height) > THUMB_DIM {
        image::RgbaImage::from_raw(width, height, first).map(|img| {
            let t = image::DynamicImage::ImageRgba8(img)
                .thumbnail(THUMB_DIM, THUMB_DIM)
                .into_rgba8();
            let (tw, th) = t.dimensions();
            ThumbData {
                width: tw,
                height: th,
                pixels: t.into_raw(),
                original_size: (width, height),
            }
        })
    } else {
        Some(ThumbData {
            width,
            height,
            pixels: first,
            original_size: (width, height),
        })
    };

    AnimatedImage {
        width,
        height,
        frames,
        thumbnail,
    }
}

/// First-frame composite as a static image (thumb fallback for archives).
pub fn first_frame_rgba(anim: &AnimatedImage) -> Vec<u8> {
    let mut canvas = FrameCanvas::new(anim.width, anim.height);
    canvas.composite_frame(&anim.frames[0]);
    canvas.pixels().to_vec()
}

// ---------------------------------------------------------------------------
// Canvas compositor
// ---------------------------------------------------------------------------

/// Canvas for compositing animation frames at display time.
///
/// Maintains a persistent pixel buffer and applies frames one at a time
/// according to their disposal methods.
pub struct FrameCanvas {
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    prev_snapshot: Vec<u8>,
}

impl FrameCanvas {
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
                        self.canvas[off..off + 4].copy_from_slice(&[0, 0, 0, 0]);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(left: u32, top: u32, w: u32, h: u32, rgba: [u8; 4]) -> RawFrame {
        RawFrame {
            left,
            top,
            width: w,
            height: h,
            pixels: rgba.repeat((w * h) as usize),
            dispose: gif::DisposalMethod::Keep,
            delay: Duration::from_millis(20),
        }
    }

    #[test]
    fn composites_subrect_at_position() {
        let mut canvas = FrameCanvas::new(2, 2);
        canvas.composite_frame(&solid_frame(1, 1, 1, 1, [9, 9, 9, 255]));
        let px = canvas.pixels();
        assert_eq!(&px[0..4], &[0, 0, 0, 0]); // untouched
        assert_eq!(&px[12..16], &[9, 9, 9, 255]); // bottom-right
    }

    #[test]
    fn background_disposal_clears_frame_rect() {
        let mut canvas = FrameCanvas::new(2, 1);
        let mut frame = solid_frame(0, 0, 2, 1, [5, 5, 5, 255]);
        frame.dispose = gif::DisposalMethod::Background;
        canvas.composite_frame(&frame);
        canvas.apply_disposal(&frame);
        assert!(canvas.pixels().iter().all(|&b| b == 0));
    }

    #[test]
    fn previous_disposal_restores_snapshot() {
        let mut canvas = FrameCanvas::new(1, 1);
        canvas.composite_frame(&solid_frame(0, 0, 1, 1, [1, 2, 3, 255]));
        let mut overlay = solid_frame(0, 0, 1, 1, [9, 9, 9, 255]);
        overlay.dispose = gif::DisposalMethod::Previous;
        canvas.composite_frame(&overlay);
        assert_eq!(&canvas.pixels()[..4], &[9, 9, 9, 255]);
        canvas.apply_disposal(&overlay);
        assert_eq!(&canvas.pixels()[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn decodes_gif_bytes_with_frames_and_thumbnail() {
        // Two-frame 4×4 GIF via the image crate's encoder.
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 4, 4, &[]).unwrap();
            for shade in [10u8, 200] {
                let mut pixels = [shade; 4 * 4 * 4].to_vec();
                // Opaque alpha.
                pixels.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
                let frame = gif::Frame::from_rgba(4, 4, &mut pixels);
                encoder.write_frame(&frame).unwrap();
            }
        }
        let anim = decode_gif_bytes(&bytes).unwrap();
        assert_eq!((anim.width, anim.height), (4, 4));
        assert_eq!(anim.frames.len(), 2);
        // Small animations use the first frame as the thumbnail directly.
        let thumb = anim.thumbnail.as_ref().expect("thumbnail always present");
        assert_eq!((thumb.width, thumb.height), (4, 4));
        assert_eq!(&thumb.pixels[..3], &[10, 10, 10]);
    }

    #[test]
    fn garbage_gif_is_an_error() {
        assert!(decode_gif_bytes(b"GIF8 not really").is_err());
    }
}
