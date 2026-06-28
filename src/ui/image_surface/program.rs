//! The iced shader-widget glue for stills and animations: a `Program` that
//! places the current frame and a `Primitive` that hands the per-draw work to the
//! pipeline. Zoom, pan, and fit reuse the shared display geometry, so stills,
//! animations, and video share one path and never diverge.

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::{ImagePipeline, Keepalive};
use crate::app::Message;
use crate::ui::image_display::display_geometry;

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
    valid: bool,
    /// Destination rect in normalized widget space: x0, y0, x1, y1.
    dst: [f32; 4],
    /// Source rect in texture UV space: u0, v0, u1, v1.
    src: [f32; 4],
    /// Nearest sampling when zoomed past 100% with crisp pixels on.
    nearest: bool,
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
        let nearest = pixelated && zoom > 1.0;
        match display_geometry(zoom, pan, viewport, original) {
            Some((dst, src)) => Self {
                texture,
                valid: true,
                dst,
                src,
                nearest,
            },
            None => Self {
                texture,
                valid: false,
                dst: [0.0; 4],
                src: [0.0; 4],
                nearest,
            },
        }
    }

    /// A degenerate surface that builds the pipeline but draws nothing.
    fn warmup() -> Self {
        Self {
            texture: None,
            valid: false,
            dst: [0.0; 4],
            src: [0.0; 4],
            nearest: false,
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
            valid: self.valid,
            dst: self.dst,
            src: self.src,
            nearest: self.nearest,
        }
    }
}

/// One frame's worth of work handed to the renderer.
pub struct ImagePrimitive {
    /// The resident texture to draw, owned for the whole frame.
    texture: Option<Keepalive>,
    valid: bool,
    dst: [f32; 4],
    src: [f32; 4],
    nearest: bool,
}

impl std::fmt::Debug for ImagePrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePrimitive")
            .field("valid", &self.valid)
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
        if self.valid {
            pipeline.write_uniforms(queue, self.dst, self.src);
        }
    }

    fn draw(&self, pipeline: &ImagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.valid
            && let Some(texture) = &self.texture
        {
            pipeline.draw_resident(render_pass, texture, self.nearest);
        }
        true
    }
}
