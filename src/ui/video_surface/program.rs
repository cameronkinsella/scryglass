//! The iced shader widget glue: a `Program` that places the current frame
//! and a `Primitive` that hands the per-frame work to the pipeline. Zoom,
//! pan, and fit reuse the still-image display math, so video and stills
//! share one geometry and never diverge.

use std::sync::Arc;

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::VideoPipeline;
use crate::app::Message;
use crate::ui::image_display::SurfacePlacement;
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
    placement: SurfacePlacement,
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
        Self {
            frame,
            placement: SurfacePlacement::new(zoom, pan, viewport, original, pixelated),
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
            placement: self.placement,
        }
    }
}

/// A single frame's worth of work handed to the renderer.
pub struct VideoPrimitive {
    frame: Arc<VideoFrame>,
    placement: SurfacePlacement,
}

impl std::fmt::Debug for VideoPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPrimitive")
            .field("frame_id", &self.frame.id)
            .field("valid", &self.placement.valid)
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
        if self.placement.valid {
            pipeline.prepare(
                device,
                queue,
                &self.frame,
                self.placement.dst,
                self.placement.src,
            );
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.placement.valid {
            pipeline.draw(render_pass, self.placement.nearest);
        }
        true
    }
}
