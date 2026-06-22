//! Video playback via FFmpeg's libraries (cargo feature `video`).
//!
//! One session thread demuxes the file in-process. Video packets decode on
//! the GPU when the platform and codec allow, otherwise on multithreaded
//! software, into planar YUV that flows through a bounded channel to the GPU
//! converter. Audio decodes and resamples to f32 PCM for a rodio sink on its
//! own thread, whose position is the master clock. Files without audio use a
//! pause-aware wall clock. Seeking opens a fresh session at the target. No
//! external processes are involved.

use std::path::Path;
use std::time::Duration;

use ffmpeg_next as ffmpeg;

mod audio;
mod decode;
mod frame;
mod hw;
mod session;
pub use frame::{VideoFrame, YuvFormat, YuvMatrix, YuvRange};
pub use session::{TempFileGuard, VideoSession, clean_extraction_dir, extraction_dir};

/// Video container extensions offered in the file list.
pub const EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v"];

/// Whether this path looks like a video file.
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Initialize FFmpeg once, with routine decoder chatter silenced, because
/// libav's default callback prints warnings (e.g. mp3float's "could not
/// update timestamps for skipped samples") straight to the console.
/// Real failures still log at error level.
fn init_ffmpeg() -> Result<(), ffmpeg::Error> {
    ffmpeg::init()?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);
    Ok(())
}

/// A decoded first frame in RGBA, with the source's native dimensions.
pub struct FirstFrame {
    pub width: u32,
    pub height: u32,
    pub native_size: (u32, u32),
    pub pixels: Vec<u8>,
}

/// Decode the first video frame of `path` as RGBA, optionally scaled to
/// fit `max_dim`. Blocking, run on a worker. Also serves AVIF stills,
/// which are AV1 keyframes in a HEIF container.
pub fn first_frame(path: &Path, max_dim: Option<u32>) -> Option<FirstFrame> {
    init_ffmpeg().ok()?;
    let mut input = ffmpeg::format::input(path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder()
        .video()
        .ok()?;

    let mut frame = ffmpeg::frame::Video::empty();
    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        if decoder.receive_frame(&mut frame).is_err() {
            continue;
        }

        let (width, height) = (decoder.width(), decoder.height());
        let scale = match max_dim {
            Some(dim) => (dim as f32 / width.max(height) as f32).min(1.0),
            None => 1.0,
        };
        let out_w = ((width as f32 * scale) as u32).max(1);
        let out_h = ((height as f32 * scale) as u32).max(1);

        let mut scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            out_w,
            out_h,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .ok()?;
        let mut rgba_frame = ffmpeg::frame::Video::empty();
        scaler.run(&frame, &mut rgba_frame).ok()?;

        let stride = rgba_frame.stride(0);
        let row_bytes = out_w as usize * 4;
        let data = rgba_frame.data(0);
        let mut pixels = Vec::with_capacity(row_bytes * out_h as usize);
        for row in 0..out_h as usize {
            let offset = row * stride;
            pixels.extend_from_slice(&data[offset..offset + row_bytes]);
        }

        return Some(FirstFrame {
            width: out_w,
            height: out_h,
            native_size: (width, height),
            pixels,
        });
    }
    None
}

/// Decode the first frame, scaled to fit `max_dim`, for the filmstrip.
/// Blocking, run on a worker.
pub fn first_frame_thumb(path: &Path, max_dim: u32) -> Option<crate::media::ThumbData> {
    let frame = first_frame(path, Some(max_dim))?;
    Some(crate::media::ThumbData {
        width: frame.width,
        height: frame.height,
        pixels: frame.pixels,
        original_size: frame.native_size,
    })
}

/// Audio output sample rate. The resampler converts everything to this.
const AUDIO_RATE: u32 = 48000;

/// A UI-poll gap this long (e.g. a window move/resize loop) counts as a
/// stall, so playback freezes instead of drifting.
const STALL: Duration = Duration::from_millis(200);
