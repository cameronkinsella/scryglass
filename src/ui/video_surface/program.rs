//! The iced shader widget glue: a `Program` that places the current frame
//! and a `Primitive` that hands the per-frame work to the pipeline. Zoom,
//! pan, and fit reuse the still-image display math, so video and stills
//! share one geometry and never diverge.

use std::sync::Arc;

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::VideoPipeline;
use crate::app::Message;
use crate::ui::image_display::{self, SurfacePlacement, snap_footprint_to_unit};
use crate::video::VideoFrame;

/// Build the video surface element for the current frame at the given zoom/pan.
/// `high_quality` selects the factor-aware downscale for a minified frame. `playing`
/// asks the compositor to redraw every display refresh so playback is vsync-paced.
/// Fills the image area like the still-image widget does.
pub fn view(
    frame: Arc<VideoFrame>,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
    high_quality: bool,
    playing: bool,
) -> Element<'static, Message> {
    shader::Shader::new(VideoSurface::new(
        frame,
        zoom,
        pan,
        viewport,
        pixelated,
        high_quality,
        playing,
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The shader program: holds the frame to show and where to put it.
struct VideoSurface {
    frame: Arc<VideoFrame>,
    placement: SurfacePlacement,
    high_quality: bool,
    playing: bool,
}

impl VideoSurface {
    fn new(
        frame: Arc<VideoFrame>,
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        pixelated: bool,
        high_quality: bool,
        playing: bool,
    ) -> Self {
        let original = (frame.width, frame.height);
        Self {
            frame,
            placement: SurfacePlacement::new(zoom, pan, viewport, original, pixelated),
            high_quality,
            playing,
        }
    }
}

impl<T> shader::Program<T> for VideoSurface {
    type State = ();
    type Primitive = VideoPrimitive;

    /// While playing, ask for a redraw on the next display refresh. iced calls this on
    /// every `RedrawRequested`, so the request renews each frame and the video draws
    /// on every vsync (the panel's own rate) with no wall-clock timer, the pacing a
    /// dedicated player uses. `poll()` advances the frame per redraw. Paused, the
    /// request stops and a slow timer handles only the control fade.
    fn update(
        &self,
        _state: &mut Self::State,
        _event: &iced::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<shader::Action<T>> {
        self.playing.then(shader::Action::request_redraw)
    }

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> VideoPrimitive {
        VideoPrimitive {
            frame: self.frame.clone(),
            placement: self.placement,
            high_quality: self.high_quality,
        }
    }
}

/// A single frame's worth of work handed to the renderer.
pub struct VideoPrimitive {
    frame: Arc<VideoFrame>,
    placement: SurfacePlacement,
    high_quality: bool,
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
        bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        if self.placement.valid {
            // Downscale ratio for the frame placed in the real widget area, taken to
            // physical pixels by the scale factor (matching the still shader). A
            // near-1:1 frame snaps to a single tap, so 1:1 playback pays no kernel.
            let vp = (bounds.width, bounds.height);
            let tex_dims = (self.frame.width, self.frame.height);
            let raw =
                image_display::footprint(self.placement.dst, self.placement.src, tex_dims, vp);
            let scale = viewport.scale_factor().max(1.0);
            let footprint = [
                snap_footprint_to_unit(raw[0] / scale),
                snap_footprint_to_unit(raw[1] / scale),
            ];
            pipeline.prepare(
                device,
                queue,
                &self.frame,
                self.placement.dst,
                self.placement.src,
                footprint,
            );
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.placement.valid {
            pipeline.draw(render_pass, self.placement.nearest, self.high_quality);
        }
        true
    }
}
