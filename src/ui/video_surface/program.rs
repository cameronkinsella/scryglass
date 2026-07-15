//! The iced shader widget glue: a `Program` that places the current frame
//! and a `Primitive` that hands the per-frame work to the pipeline. Zoom,
//! pan, and fit reuse the still-image display math, so video and stills
//! share one geometry and never diverge.

use std::sync::Arc;

use iced::widget::shader;
use iced::{Element, Length, Rectangle, Size, mouse, wgpu};

use super::pipeline::VideoPipeline;
use crate::app::Message;
use crate::app::viewer_math::compute_zoom;
use crate::config::ZoomMode;
use crate::ui::geometry::{self, SurfacePlacement, snap_footprint_to_unit};
use crate::video::VideoFrame;

/// Build the video surface element for the current frame at the given zoom/pan.
/// `high_quality` selects the factor-aware downscale for a minified frame.
/// Fills the image area like the still-image widget does.
#[allow(clippy::too_many_arguments)]
pub fn view(
    frame: Arc<VideoFrame>,
    session: u64,
    zoom: f32,
    pan: (f32, f32),
    pixelated: bool,
    high_quality: bool,
    zoom_mode: ZoomMode,
    manual_zoom: bool,
) -> Element<'static, Message> {
    shader::Shader::new(VideoSurface {
        frame,
        session,
        zoom,
        pan,
        pixelated,
        zoom_mode,
        manual_zoom,
        high_quality,
    })
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The shader program: holds the frame to show and the zoom state that
/// places it. The placement itself is resolved at draw time from iced's
/// real widget size, like the still surface, so a live resize never draws
/// a frame-stale geometry.
struct VideoSurface {
    frame: Arc<VideoFrame>,
    /// The session the frame belongs to, keying its plane textures so two
    /// windows playing at once never fight over one texture slot.
    session: u64,
    zoom: f32,
    pan: (f32, f32),
    pixelated: bool,
    zoom_mode: ZoomMode,
    manual_zoom: bool,
    high_quality: bool,
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
            session: self.session,
            zoom: self.zoom,
            pan: self.pan,
            zoom_mode: self.zoom_mode,
            manual_zoom: self.manual_zoom,
            high_quality: self.high_quality,
            pixelated: self.pixelated,
        }
    }
}

/// A single frame's worth of work handed to the renderer.
pub struct VideoPrimitive {
    frame: Arc<VideoFrame>,
    session: u64,
    zoom: f32,
    pan: (f32, f32),
    zoom_mode: ZoomMode,
    manual_zoom: bool,
    high_quality: bool,
    pixelated: bool,
}

impl VideoPrimitive {
    /// The zoom the frame actually draws at in `viewport`: the manual zoom,
    /// or the fit recomputed from the render-time size, like the placement.
    fn resolved_zoom(&self, viewport: (f32, f32)) -> f32 {
        if self.manual_zoom {
            self.zoom
        } else {
            compute_zoom(
                self.zoom_mode,
                self.frame.width,
                self.frame.height,
                Size::new(viewport.0, viewport.1),
            )
        }
    }

    /// Resolve the placement for the render-time image-area size (`bounds`).
    fn placement(&self, viewport: (f32, f32)) -> SurfacePlacement {
        let original = (self.frame.width, self.frame.height);
        SurfacePlacement::new(self.resolved_zoom(viewport), self.pan, viewport, original)
    }
}

impl std::fmt::Debug for VideoPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPrimitive")
            .field("frame_id", &self.frame.id)
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
        // Resolve the placement against iced's real widget size now, so the
        // geometry matches the size actually drawn into rather than the app's
        // viewport estimate, which lags a frame behind during a resize.
        let vp = (bounds.width, bounds.height);
        let placement = self.placement(vp);
        if placement.valid {
            // Downscale ratio for the frame placed in the real widget area, taken to
            // physical pixels by the scale factor (matching the still shader). A
            // near-1:1 frame snaps to a single tap, so 1:1 playback pays no kernel.
            let tex_dims = (self.frame.width, self.frame.height);
            let raw = geometry::footprint(placement.dst, placement.src, tex_dims, vp);
            let scale = viewport.scale_factor().max(1.0);
            let footprint = [
                snap_footprint_to_unit(raw[0] / scale),
                snap_footprint_to_unit(raw[1] / scale),
            ];
            // Nearest sampling keys off the zoom the frame actually draws at,
            // resolved from the same render-time bounds as the placement. The
            // update-side estimate lags during a live resize and could pick
            // the wrong sampler for a fit zoom straddling 1.0. Stored on the
            // pipeline (prepare cannot reach draw through the primitive).
            let nearest = self.pixelated && self.resolved_zoom(vp) > 1.0;
            pipeline.prepare(
                device,
                queue,
                self.session,
                &self.frame,
                placement.dst,
                placement.src,
                footprint,
                nearest,
            );
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.frame.width > 0 && self.frame.height > 0 && self.zoom > 0.0 {
            pipeline.draw(render_pass, self.session, self.high_quality);
        }
        true
    }
}
