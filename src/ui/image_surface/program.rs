//! The iced shader-widget glue for stills: a `Program` that places the current
//! image and a `Primitive` that hands the per-draw work to the pipeline. Zoom,
//! pan, and fit reuse the shared display geometry, so stills and video share
//! one path and never diverge.

use iced::widget::image::Handle;
use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::{ImagePipeline, Keepalive};
use crate::app::Message;
use crate::ui::image_display::display_geometry;

/// Build the still-image surface element for `handle` at the given zoom/pan.
/// Fills the image area like the placeholder and video paths do.
///
/// When `texture` is `Some`, the surface renders that resident texture directly
/// (the display owns it, so a black screen is impossible). `None` falls back to
/// the id→texture map for the animation/bootstrap paths that don't hold a keepalive.
pub fn view(
    handle: Handle,
    texture: Option<Keepalive>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
) -> Element<'static, Message> {
    shader::Shader::new(ImageSurface::new(
        handle, texture, original, zoom, pan, viewport, pixelated,
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

/// The shader program: the image to show and where to place it.
struct ImageSurface {
    handle: Handle,
    /// The resident texture the display owns, drawn directly when present.
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
        handle: Handle,
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
                handle,
                texture,
                valid: true,
                dst,
                src,
                nearest,
            },
            None => Self {
                handle,
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
            handle: Handle::from_rgba(1, 1, vec![0, 0, 0, 0]),
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
            handle: self.handle.clone(),
            texture: self.texture.clone(),
            valid: self.valid,
            dst: self.dst,
            src: self.src,
            nearest: self.nearest,
        }
    }
}

/// One still's worth of work handed to the renderer.
pub struct ImagePrimitive {
    handle: Handle,
    /// The resident texture to draw, owned for the whole frame. `None` falls back
    /// to the id→texture map (animation/bootstrap).
    texture: Option<Keepalive>,
    valid: bool,
    dst: [f32; 4],
    src: [f32; 4],
    nearest: bool,
}

impl std::fmt::Debug for ImagePrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePrimitive")
            .field("id", &self.handle.id())
            .field("valid", &self.valid)
            .finish()
    }
}

impl shader::Primitive for ImagePrimitive {
    type Pipeline = ImagePipeline;

    fn prepare(
        &self,
        pipeline: &mut ImagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if !self.valid {
            return;
        }
        // A held texture only needs the uniforms written; the id→texture map (and
        // its inline upload) is the fallback for the keepalive-less paths.
        match &self.texture {
            Some(_) => pipeline.write_uniforms(queue, self.dst, self.src),
            None => pipeline.prepare(device, queue, &self.handle, self.dst, self.src),
        }
    }

    fn draw(&self, pipeline: &ImagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.valid {
            match &self.texture {
                Some(texture) => pipeline.draw_resident(render_pass, texture, self.nearest),
                None => pipeline.draw(render_pass, self.nearest),
            }
        }
        true
    }
}
