//! The iced shader widget glue: a `Program` that places the current frame
//! and a `Primitive` that hands the per-frame work to the pipeline. Zoom,
//! pan, and fit reuse the still-image display math, so video and stills
//! share one geometry and never diverge.

use std::sync::Arc;

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::geometry::geometry;
use super::pipeline::VideoPipeline;
use crate::app::Message;
use crate::video::VideoFrame;

/// Build the video surface element for the current frame at the given
/// zoom/pan. Fills the image area like the still-image widget does.
pub fn view(
    frame: Arc<VideoFrame>,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
) -> Element<'static, Message> {
    shader::Shader::new(VideoSurface::new(frame, zoom, pan, viewport, pixelated))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The shader program: holds the frame to show and where to put it.
struct VideoSurface {
    frame: Arc<VideoFrame>,
    valid: bool,
    /// Destination rect in normalized widget space: x0, y0, x1, y1.
    dst: [f32; 4],
    /// Source rect in texture UV space: u0, v0, u1, v1.
    src: [f32; 4],
    /// Nearest sampling when zoomed past 100% with crisp pixels on.
    nearest: bool,
}

impl VideoSurface {
    fn new(
        frame: Arc<VideoFrame>,
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        pixelated: bool,
    ) -> Self {
        let original = (frame.width, frame.height);
        let nearest = pixelated && zoom > 1.0;
        match geometry(zoom, pan, viewport, original) {
            Some((dst, src)) => Self {
                frame,
                valid: true,
                dst,
                src,
                nearest,
            },
            None => Self {
                frame,
                valid: false,
                dst: [0.0; 4],
                src: [0.0; 4],
                nearest,
            },
        }
    }
}

impl<T> shader::Program<T> for VideoSurface {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> VideoPrimitive {
        VideoPrimitive {
            frame: self.frame.clone(),
            valid: self.valid,
            dst: self.dst,
            src: self.src,
            nearest: self.nearest,
        }
    }
}

/// A single frame's worth of work handed to the renderer.
pub struct VideoPrimitive {
    frame: Arc<VideoFrame>,
    valid: bool,
    dst: [f32; 4],
    src: [f32; 4],
    nearest: bool,
}

impl std::fmt::Debug for VideoPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPrimitive")
            .field("frame_id", &self.frame.id)
            .field("valid", &self.valid)
            .finish()
    }
}

impl shader::Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut VideoPipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if self.valid {
            pipeline.prepare(device, queue, &self.frame, self.dst, self.src);
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.valid {
            pipeline.draw(render_pass, self.nearest);
        }
        true
    }
}
