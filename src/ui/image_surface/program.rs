//! The iced shader-widget glue for stills and animations: a `Program` that
//! places the current frame and a `Primitive` that hands the per-draw work to the
//! pipeline. Zoom, pan, and fit reuse the shared display geometry, so stills,
//! animations, and video share one path and never diverge.

use std::sync::{Arc, Weak};

use iced::widget::shader;
use iced::{Element, Length, Rectangle, Size, mouse, wgpu};

use super::pipeline::{ImagePipeline, Keepalive, ResidentImage};
use crate::app::Message;
use crate::app::viewer_math::compute_zoom;
use crate::config::{DownscaleKernel, ZoomMode};
use crate::ui::geometry::{
    self, SurfacePlacement, near_one_to_one, snap_footprint_to_unit, snap_placement_to_pixels,
};

/// Build the image surface element at the given zoom/pan, drawing the resident
/// `texture` directly (read live from the store's shared cell for stills, or the
/// current frame's keepalive for animations, so a black screen is impossible).
/// `None` is the degenerate warmup case that draws nothing. Fills the image area
/// like the placeholder and video paths do.
///
/// The geometry is resolved at draw time from iced's real widget size, not from the
/// app's viewport estimate, so a fast resize never draws a frame-stale placement.
#[allow(clippy::too_many_arguments)]
pub fn view(
    window: iced::window::Id,
    texture: Option<Keepalive>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    pixelated: bool,
    kernel: DownscaleKernel,
    zoom_mode: ZoomMode,
    manual_zoom: bool,
) -> Element<'static, Message> {
    shader::Shader::new(ImageSurface::new(
        window,
        texture,
        original,
        zoom,
        pan,
        pixelated,
        kernel,
        zoom_mode,
        manual_zoom,
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

/// The shader program: the texture to show plus the raw zoom/pan inputs. The
/// placement is resolved per frame in `prepare` from iced's real widget size, not
/// stored here, so it can never lag the size actually being drawn into.
struct ImageSurface {
    /// The window this surface draws in, so a tiled draw stamps under its own
    /// id and the demand pass reads back this window's own draw.
    window: iced::window::Id,
    /// The resident texture to draw, or `None` for the warmup surface. Weak,
    /// because iced keeps the last widget tree and primitive of a window that
    /// stops redrawing (a minimized one): a strong ref here would pin VRAM the
    /// decay freed. The store owns the texture. Draws upgrade per frame.
    texture: Option<Weak<ResidentImage>>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    kernel: DownscaleKernel,
    zoom_mode: ZoomMode,
    manual_zoom: bool,
    /// The resident texture's texel size, fed into the shader uniform.
    tex_size: [f32; 2],
    /// Crisp-pixel magnification, decided from the zoom (never above 1 for a fit),
    /// so `draw` can pick the sampler without the render-time size.
    nearest: bool,
}

impl ImageSurface {
    #[allow(clippy::too_many_arguments)]
    fn new(
        window: iced::window::Id,
        texture: Option<Keepalive>,
        original: (u32, u32),
        zoom: f32,
        pan: (f32, f32),
        pixelated: bool,
        kernel: DownscaleKernel,
        zoom_mode: ZoomMode,
        manual_zoom: bool,
    ) -> Self {
        // The texel size comes from the resident texture (a view-res copy is already
        // smaller) or a pyramid's substrate (a budget-clamped decode is smaller than
        // the true dims), falling back to the original dims for the tokenless test
        // keepalive.
        let tex_dims = texture
            .as_ref()
            .and_then(|t| t.size().or_else(|| t.tiles().map(|set| set.original())))
            .unwrap_or(original);
        Self {
            window,
            texture: texture.as_ref().map(Arc::downgrade),
            original,
            zoom,
            pan,
            kernel,
            zoom_mode,
            manual_zoom,
            tex_size: [tex_dims.0 as f32, tex_dims.1 as f32],
            nearest: pixelated && zoom > 1.0,
        }
    }

    /// A degenerate surface that builds the pipeline but draws nothing.
    fn warmup() -> Self {
        Self {
            // Warmup never reaches the tiled prepare path, so this id is unused.
            window: iced::window::Id::unique(),
            texture: None,
            original: (0, 0),
            zoom: 1.0,
            pan: (0.0, 0.0),
            kernel: DownscaleKernel::default(),
            zoom_mode: ZoomMode::default(),
            manual_zoom: false,
            tex_size: [1.0, 1.0],
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
            window: self.window,
            texture: self.texture.clone(),
            original: self.original,
            zoom: self.zoom,
            pan: self.pan,
            kernel: self.kernel,
            zoom_mode: self.zoom_mode,
            manual_zoom: self.manual_zoom,
            tex_size: self.tex_size,
            nearest: self.nearest,
        }
    }
}

/// One frame's worth of work handed to the renderer.
pub struct ImagePrimitive {
    /// The window this draw belongs to, so a tiled prepare stamps under its
    /// own id and the demand pass reads back this window's own draw.
    window: iced::window::Id,
    /// The resident texture to draw. Weak like the surface's (iced retains the
    /// last primitive of a window that stops redrawing). Prepare and draw
    /// upgrade it, and the store cannot free it between them because no update
    /// runs mid-frame.
    texture: Option<Weak<ResidentImage>>,
    original: (u32, u32),
    zoom: f32,
    pan: (f32, f32),
    kernel: DownscaleKernel,
    zoom_mode: ZoomMode,
    manual_zoom: bool,
    tex_size: [f32; 2],
    nearest: bool,
}

impl ImagePrimitive {
    /// Resolve the placement for the render-time image-area size (`bounds`).
    fn placement(&self, viewport: (f32, f32)) -> SurfacePlacement {
        let zoom = if self.manual_zoom {
            self.zoom
        } else {
            compute_zoom(
                self.zoom_mode,
                self.original.0,
                self.original.1,
                Size::new(viewport.0, viewport.1),
            )
        };
        SurfacePlacement::new(zoom, self.pan, viewport, self.original)
    }
}

impl std::fmt::Debug for ImagePrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePrimitive")
            .field("original", &self.original)
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
        bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        pipeline.record_scale_factor(viewport.scale_factor());
        // Resolve the placement against iced's real widget size now, so the geometry
        // matches the size actually being drawn into rather than the app's viewport
        // estimate, which lags a frame behind during a resize.
        let vp = (bounds.width, bounds.height);
        let placement = self.placement(vp);
        if placement.valid {
            // The footprint is measured in logical pixels, but the framebuffer is
            // physical, so divide by the scale factor to get texels per physical
            // pixel. Snap a near-1:1 footprint to a single exact tap so a view-res
            // copy shown at its baked size skips a redundant kernel pass.
            let tex_dims = (self.tex_size[0] as u32, self.tex_size[1] as u32);
            let raw = geometry::footprint(placement.dst, placement.src, tex_dims, vp);
            let scale = viewport.scale_factor().max(1.0);
            let footprint = [
                snap_footprint_to_unit(raw[0] / scale),
                snap_footprint_to_unit(raw[1] / scale),
            ];
            // Snap the placement to whole physical pixels so a view-res demote lands
            // on the same grid as the full-res it replaces. A near-1:1 copy (a demote
            // at its baked size, or a 100%-zoom image) also snaps its source to texel
            // centers, so the single tap is a pixel-exact copy rather than a softening
            // sub-pixel resample (worst on text). The image area is `bounds` in
            // logical pixels, taken to physical by the scale factor. The widget's
            // own offset rides in as the origin: the render pass viewport sits at
            // the unrounded physical bounds, so the chrome above the image makes
            // the framebuffer position fractional and the snap must cancel it.
            let origin = (bounds.x * scale, bounds.y * scale);
            let (dst, src) = snap_placement_to_pixels(
                placement.dst,
                placement.src,
                (self.tex_size[0], self.tex_size[1]),
                (bounds.width * scale, bounds.height * scale),
                origin,
                near_one_to_one(footprint),
            );
            // A dead ref is a texture the decay freed while this window kept
            // its stale frame state (a minimized window never redraws), so
            // draw nothing rather than pin or sample it.
            let texture = match self.texture.as_ref().map(Weak::upgrade) {
                Some(None) => {
                    pipeline.clear_tiles();
                    return;
                }
                Some(Some(t)) => Some(t),
                None => None,
            };
            // A tiled still resolves a per-tile draw list instead of one quad.
            // Its `tex_size` is the substrate, so the footprint above already
            // measures substrate texels per pixel.
            if let Some(set) = texture.as_ref().and_then(|t| t.tiles()) {
                pipeline.prepare_tiles(
                    queue,
                    self.window,
                    set,
                    dst,
                    src,
                    raw,
                    scale,
                    (bounds.width * scale, bounds.height * scale),
                    origin,
                    self.kernel,
                );
                return;
            }
            pipeline.clear_tiles();
            pipeline.write_uniforms(queue, 0, dst, src, footprint, self.tex_size, self.kernel);
        } else {
            pipeline.clear_tiles();
        }
    }

    fn draw(&self, pipeline: &ImagePipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if let Some(texture) = self.texture.as_ref().and_then(Weak::upgrade) {
            if texture.tiles().is_some() {
                pipeline.draw_tiles(render_pass, self.nearest);
            } else {
                pipeline.draw_resident(render_pass, &texture, self.nearest);
            }
        }
        true
    }
}
