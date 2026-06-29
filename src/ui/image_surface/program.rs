//! The iced shader-widget glue for stills and animations: a `Program` that
//! places the current frame and a `Primitive` that hands the per-draw work to the
//! pipeline. Zoom, pan, and fit reuse the shared display geometry, so stills,
//! animations, and video share one path and never diverge.

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use super::pipeline::{ImagePipeline, Keepalive};
use crate::app::Message;
use crate::config::DownscaleKernel;
use crate::ui::image_display::{self, SurfacePlacement};

/// Build the image surface element at the given zoom/pan, drawing the resident
/// `texture` directly (read live from the store's shared cell for stills, or the
/// current frame's keepalive for animations, so a black screen is impossible).
/// `None` is the degenerate warmup case that draws nothing. Fills the image area
/// like the placeholder and video paths do.
#[allow(clippy::too_many_arguments)]
pub fn view(
    texture: Option<Keepalive>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
    kernel: DownscaleKernel,
) -> Element<'static, Message> {
    shader::Shader::new(ImageSurface::new(
        texture, original, zoom, pan, viewport, pixelated, kernel,
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

/// The shader program: the texture to show, where to place it, and how to downscale.
struct ImageSurface {
    /// The resident texture to draw, or `None` for the warmup surface.
    texture: Option<Keepalive>,
    placement: SurfacePlacement,
    /// The downscale kernel and the per-axis footprint it is scaled by, plus the
    /// texture's texel size, all passed straight into the shader uniform.
    footprint: [f32; 2],
    tex_size: [f32; 2],
    kernel: u32,
    bc: [f32; 2],
}

impl ImageSurface {
    #[allow(clippy::too_many_arguments)]
    fn new(
        texture: Option<Keepalive>,
        original: (u32, u32),
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        pixelated: bool,
        kernel: DownscaleKernel,
    ) -> Self {
        let placement = SurfacePlacement::new(zoom, pan, viewport, original, pixelated);
        // The footprint is measured against the resident texture's real size (a
        // view-res copy is already smaller, so its footprint is nearer 1), falling
        // back to the original dims for the tokenless test keepalive.
        let tex_dims = texture.as_ref().and_then(|t| t.size()).unwrap_or(original);
        let footprint = if placement.valid {
            image_display::footprint(placement.dst, placement.src, tex_dims, viewport)
        } else {
            [1.0, 1.0]
        };
        let (selector, bc) = kernel.shader_params();
        Self {
            texture,
            placement,
            footprint,
            tex_size: [tex_dims.0 as f32, tex_dims.1 as f32],
            kernel: selector,
            bc,
        }
    }

    /// A degenerate surface that builds the pipeline but draws nothing.
    fn warmup() -> Self {
        Self {
            texture: None,
            placement: SurfacePlacement::empty(),
            footprint: [1.0, 1.0],
            tex_size: [1.0, 1.0],
            kernel: 0,
            bc: [0.0, 0.0],
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
            footprint: self.footprint,
            tex_size: self.tex_size,
            kernel: self.kernel,
            bc: self.bc,
        }
    }
}

/// One frame's worth of work handed to the renderer.
pub struct ImagePrimitive {
    /// The resident texture to draw, owned for the whole frame.
    texture: Option<Keepalive>,
    placement: SurfacePlacement,
    footprint: [f32; 2],
    tex_size: [f32; 2],
    kernel: u32,
    bc: [f32; 2],
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
            pipeline.write_uniforms(
                queue,
                self.placement.dst,
                self.placement.src,
                self.footprint,
                self.tex_size,
                self.kernel,
                self.bc,
            );
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
