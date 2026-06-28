//! The iced shader-widget glue for stills and animations: a `Program` that
//! places the current frame and a `Primitive` that hands the per-draw work to the
//! pipeline. Zoom, pan, and fit reuse the shared display geometry, so stills,
//! animations, and video share one path and never diverge.

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::{ImagePipeline, Keepalive};
use crate::app::Message;
use crate::ui::image_display::SurfacePlacement;

/// Build the image surface element at the given zoom/pan, drawing the resident
/// `texture` directly (read live from the store's shared cell for stills, or the
/// current frame's keepalive for animations, so a black screen is impossible).
/// `None` is the degenerate warmup case that draws nothing. Fills the image area
/// like the placeholder and video paths do.
pub fn view(
    texture: Option<Keepalive>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
) -> Element<'static, Message> {
    shader::Shader::new(ImageSurface::new(
        texture, original, zoom, pan, viewport, pixelated,
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// A no-op surface that draws nothing but makes iced build the image pipeline
/// (and its upload thread) up front, so an early decode never races the worker.
pub fn warmup() -> Element<'static, Message> {
    shader::Shader::new(ImageSurface::warmup())
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The shader program: the texture to show and where to place it.
struct ImageSurface {
    /// The resident texture to draw, or `None` for the warmup surface.
    texture: Option<Keepalive>,
    placement: SurfacePlacement,
}

impl ImageSurface {
    fn new(
        texture: Option<Keepalive>,
        original: (u32, u32),
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        pixelated: bool,
    ) -> Self {
        Self {
            texture,
            placement: SurfacePlacement::new(zoom, pan, viewport, original, pixelated),
        }
    }

    /// A degenerate surface that builds the pipeline but draws nothing.
    fn warmup() -> Self {
        Self {
            texture: None,
            placement: SurfacePlacement::empty(),
        }
    }
}

impl<T> shader::Program<T> for ImageSurface {
    type State = ();
    type Primitive = ImagePrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> ImagePrimitive {
        ImagePrimitive {
            texture: self.texture.clone(),
            placement: self.placement,
        }
    }
}

/// One frame's worth of work handed to the renderer.
pub struct ImagePrimitive {
    /// The resident texture to draw, owned for the whole frame.
    texture: Option<Keepalive>,
    placement: SurfacePlacement,
}

impl std::fmt::Debug for ImagePrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePrimitive")
            .field("valid", &self.placement.valid)
            .finish()
    }
}

impl shader::Primitive for ImagePrimitive {
    type Pipeline = ImagePipeline;

    fn prepare(
        &self,
        pipeline: &mut ImagePipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if self.placement.valid {
            pipeline.write_uniforms(queue, self.placement.dst, self.placement.src);
        }
    }

    fn draw(&self, pipeline: &ImagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.placement.valid
            && let Some(texture) = &self.texture
        {
            pipeline.draw_resident(render_pass, texture, self.placement.nearest);
        }
        true
    }
}
