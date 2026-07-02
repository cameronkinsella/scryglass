//! Persistent GPU state for the still-image surface: the render pipeline and
//! the per-draw prepare/draw paths. Textures themselves are app-owned
//! `Keepalive`s drawn directly, not held here.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use iced::wgpu;
use iced::widget::shader;
use tokio::sync::mpsc::unbounded_channel;

use crate::config::DownscaleKernel;
use crate::media::tiles::TileKey;
use crate::ui::geometry::snap_footprint_to_unit;

pub(super) use super::resident::{Keepalive, ResidentImage};
use super::tiles::{DrawWant, TileSet};
use super::uniforms::{
    UNIFORM_SIZE, UNIFORM_SLOTS, UNIFORM_STRIDE, build_uniforms, tile_placement,
};
use super::upload::{
    UPLOAD_CONTEXT, UploadContext, UploadThread, sampler_desc, spawn_upload_thread,
};

/// Persistent GPU state shared by every still-image draw.
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    /// Bound at slot 0. rust-gpu reserves set 0, so the real bindings are set 1.
    empty_bind: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    is_srgb: bool,
    /// The tiled draw list `prepare` resolved for this frame: each entry is a
    /// tile (or the base layer) and the uniform slot holding its rects. Weak,
    /// so a minimized window's stale list never pins tile VRAM the decay
    /// tiers released. The pyramid holds the strong references, and no update
    /// runs between prepare and draw to drop them mid-frame.
    tile_draws: Vec<(std::sync::Weak<ResidentImage>, u32)>,
}

impl shader::Pipeline for ImagePipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scryglass image rgba"),
            source: wgpu::ShaderSource::SpirV(wgpu::util::make_spirv_raw(IMAGE_SPV)),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scryglass image bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        // A tiled draw issues one draw per tile from the same
                        // pass, each reading its own slot of the uniform
                        // buffer via a dynamic offset.
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Empty layout for set 0 (rust-gpu reserves it).
        let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scryglass image empty set"),
            entries: &[],
        });
        let empty_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scryglass image empty set"),
            layout: &empty_layout,
            entries: &[],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scryglass image pipeline layout"),
            bind_group_layouts: &[&empty_layout, &layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scryglass image pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // A second pipeline that draws into an Rgba8Unorm texture, for baking a
        // view-res copy through the same shader. Its target is not sRGB, so the
        // shader is told to write sRGB-encoded values (like every stored image).
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scryglass image view-res pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scryglass image uniforms"),
            size: UNIFORM_STRIDE * UNIFORM_SLOTS,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // A separate uniform for the view-res render, since it runs on the upload
        // thread while the display uniform is being written on the render thread.
        let render_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scryglass image view-res uniforms"),
            size: UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler_linear = device.create_sampler(&sampler_desc(wgpu::FilterMode::Linear));
        let sampler_nearest = device.create_sampler(&sampler_desc(wgpu::FilterMode::Nearest));

        // Capture the device here (the only place wgpu exposes it) and spawn
        // the dedicated upload thread. It owns the cloned device/queue, drains
        // jobs serially, and feeds finished textures to prepare via `receiver`.
        let (jobs, jobs_rx) = unbounded_channel();
        spawn_upload_thread(
            UploadThread {
                device: device.clone(),
                queue: queue.clone(),
                layout: layout.clone(),
                uniforms: uniforms.clone(),
                sampler_linear: sampler_linear.clone(),
                sampler_nearest: sampler_nearest.clone(),
                render_pipeline,
                render_uniforms,
                empty_bind: empty_bind.clone(),
            },
            jobs_rx,
            jobs.clone(),
        );
        let _ = UPLOAD_CONTEXT.set(UploadContext {
            jobs,
            max_dim: device.limits().max_texture_dimension_2d,
            kernel: AtomicU8::new(DownscaleKernel::default().to_u8()),
            scale_factor: AtomicU32::new(1.0f32.to_bits()),
        });

        Self {
            pipeline,
            empty_bind,
            uniforms,
            is_srgb: format.is_srgb(),
            tile_draws: Vec::new(),
        }
    }
}

impl ImagePipeline {
    /// Write the per-draw uniforms for the resident-texture draw path: the dst/src
    /// rects plus the downscale kernel and the footprint it is scaled by. Also
    /// records the kernel so an off-thread view-res render bakes with the same one.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_uniforms(
        &self,
        queue: &wgpu::Queue,
        slot: u32,
        dst: [f32; 4],
        src: [f32; 4],
        footprint: [f32; 2],
        tex_size: [f32; 2],
        kernel: DownscaleKernel,
    ) {
        if let Some(ctx) = UPLOAD_CONTEXT.get() {
            ctx.kernel.store(kernel.to_u8(), Ordering::Relaxed);
        }
        let (selector, bc) = kernel.shader_params();
        queue.write_buffer(
            &self.uniforms,
            slot as u64 * UNIFORM_STRIDE,
            &build_uniforms(dst, src, self.is_srgb, selector, footprint, tex_size, bc),
        );
    }

    /// Record the display scale factor a draw reported, so an off-thread view-res
    /// render sizes its copy to the physical display.
    pub(super) fn record_scale_factor(&self, scale_factor: f32) {
        if let Some(ctx) = UPLOAD_CONTEXT.get() {
            ctx.scale_factor
                .store(scale_factor.to_bits(), Ordering::Relaxed);
        }
    }

    /// Draw a resident image the caller already owns (its `Keepalive` keeps the
    /// texture alive for the whole frame), bypassing the id→texture map. This is
    /// what makes a black screen unrepresentable: the window renders the texture it
    /// holds, so nothing elsewhere can free it out from under the draw.
    pub(super) fn draw_resident(
        &self,
        render_pass: &mut wgpu::RenderPass<'_>,
        resident: &ResidentImage,
        nearest: bool,
    ) {
        let Some(bind) = resident.bind(nearest) else {
            return;
        };
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.empty_bind, &[]);
        render_pass.set_bind_group(1, bind, &[0]);
        render_pass.draw(0..6, 0..1);
    }

    /// Resolve this frame's draw list for a tiled still: the base layer first
    /// (stretched under everything, so a missing tile shows the view-quality
    /// copy instead of a hole), then every resident visible tile at the
    /// zoom's level, each in its own uniform slot. Stamps what it selected on
    /// the set, making the draw the single authority the demand pass follows.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_tiles(
        &mut self,
        queue: &wgpu::Queue,
        set: &TileSet,
        dst: [f32; 4],
        src: [f32; 4],
        raw_footprint: [f32; 2],
        scale: f32,
        viewport_phys: (f32, f32),
        kernel: DownscaleKernel,
    ) {
        use crate::media::tiles;
        self.tile_draws.clear();
        let original = set.original();
        set.stamp_draw_scale(scale);
        // The physical size the WHOLE image is displayed at. Demand targets
        // it. When zoomed in, `dst` is clipped to the viewport and `src`
        // holds the visible fraction, so the full extent is their ratio.
        let src_span = (src[2] - src[0], src[3] - src[1]);
        let shown = if src_span.0 > 0.0 && src_span.1 > 0.0 {
            (
                ((dst[2] - dst[0]) * viewport_phys.0 / src_span.0).round() as u32,
                ((dst[3] - dst[1]) * viewport_phys.1 / src_span.1).round() as u32,
            )
        } else {
            (0, 0)
        };
        set.stamp_draw_shown(shown);
        // Texels per physical pixel of the substrate. Its inverse is the
        // physical zoom the LOD is chosen for.
        let footprint = [raw_footprint[0] / scale, raw_footprint[1] / scale];
        if let Some(base) = set.base()
            && let Some((bw, bh)) = base.size()
        {
            let base_fp = [
                snap_footprint_to_unit(footprint[0] * bw as f32 / original.0 as f32),
                snap_footprint_to_unit(footprint[1] * bh as f32 / original.1 as f32),
            ];
            // A base at its shown size aligns its source to its own texel
            // grid, the exact snap the View tier gets, so its single taps
            // are texel-exact even when only part of the image is visible.
            let (bdst, bsrc) = crate::ui::geometry::snap_placement_to_pixels(
                dst,
                src,
                (bw as f32, bh as f32),
                viewport_phys,
                crate::ui::geometry::near_one_to_one(base_fp),
            );
            self.write_uniforms(
                queue,
                0,
                bdst,
                bsrc,
                base_fp,
                [bw as f32, bh as f32],
                kernel,
            );
            self.tile_draws.push((Arc::downgrade(&base), 0));
            // Exact-scale tiles cover the base wherever they have landed:
            // each is a byte-exact crop of the one-pass downscale at the
            // shown size, placed on integer pixels and drawn as aligned
            // single taps, indistinguishable from a whole exact base.
            // Re-snapping against the exact grid keeps a panned view's taps
            // on texel centers, and each tile maps through the visible
            // `src` window (dst only spans the viewport when zoomed in).
            if shown != (0, 0) && set.exact_target() == shown {
                let (edst, esrc) = crate::ui::geometry::snap_placement_to_pixels(
                    dst,
                    src,
                    (shown.0 as f32, shown.1 as f32),
                    viewport_phys,
                    true,
                );
                let espan = (edst[2] - edst[0], edst[3] - edst[1]);
                let esrc_span = ((esrc[2] - esrc[0]).max(1e-6), (esrc[3] - esrc[1]).max(1e-6));
                let mut slot = 1u32;
                for (col, row) in tiles::window_tiles(esrc, shown) {
                    if u64::from(slot) >= UNIFORM_SLOTS {
                        break;
                    }
                    let key = TileKey { lod: 0, col, row };
                    let Some(tile) = set.exact_get(shown, key) else {
                        continue;
                    };
                    let (x, y, w, h) = tiles::tile_rect(shown, col, row);
                    let fx =
                        |v: f32| edst[0] + (v / shown.0 as f32 - esrc[0]) / esrc_span.0 * espan.0;
                    let fy =
                        |v: f32| edst[1] + (v / shown.1 as f32 - esrc[1]) / esrc_span.1 * espan.1;
                    let tdst = [
                        fx(x as f32),
                        fy(y as f32),
                        fx((x + w) as f32),
                        fy((y + h) as f32),
                    ];
                    self.write_uniforms(
                        queue,
                        slot,
                        tdst,
                        [0.0, 0.0, 1.0, 1.0],
                        [1.0, 1.0],
                        [w as f32, h as f32],
                        kernel,
                    );
                    self.tile_draws.push((Arc::downgrade(&tile), slot));
                    slot += 1;
                }
                set.stamp_draw_lod(DrawWant::BaseOnly);
                return;
            }
            // The base covers the view when it is at least as fine as the
            // display OR within the near-1:1 band: float dust leaves an
            // exact base a hair either side of 1.0, and the snap only caps
            // from above, so a bare >= 1.0 test lets one axis reading
            // 0.9999 invite cascade tiles over an exact copy.
            if crate::ui::geometry::near_one_to_one(base_fp)
                || (base_fp[0] >= 1.0 && base_fp[1] >= 1.0)
            {
                set.stamp_draw_lod(DrawWant::BaseOnly);
                return;
            }
        }
        let lod = tiles::lod_for_zoom(1.0 / footprint[0].max(footprint[1]));
        set.stamp_draw_lod(DrawWant::Level(lod));
        let level = tiles::level_size(original, lod);
        let tile_fp = [
            snap_footprint_to_unit(footprint[0] * level.0 as f32 / original.0 as f32),
            snap_footprint_to_unit(footprint[1] * level.1 as f32 / original.1 as f32),
        ];
        let mut slot = 1u32;
        for key in tiles::visible_tiles(src, original, lod) {
            if u64::from(slot) >= UNIFORM_SLOTS {
                break;
            }
            let Some(tile) = set.get(key) else {
                continue;
            };
            let Some(tex) = tile.size() else {
                continue;
            };
            let (tdst, tsrc) = tile_placement(dst, src, level, key, tex);
            self.write_uniforms(
                queue,
                slot,
                tdst,
                tsrc,
                tile_fp,
                [tex.0 as f32, tex.1 as f32],
                kernel,
            );
            self.tile_draws.push((Arc::downgrade(&tile), slot));
            slot += 1;
        }
    }

    /// Release the previous frame's tile draw list. Called by the non-tiled
    /// prepare paths, else the last tiled image's tiles stay pinned in VRAM
    /// after navigating away.
    pub(super) fn clear_tiles(&mut self) {
        self.tile_draws.clear();
    }

    /// Draw the list `prepare_tiles` resolved: base first, tiles over it.
    pub(super) fn draw_tiles(&self, render_pass: &mut wgpu::RenderPass<'_>, nearest: bool) {
        if self.tile_draws.is_empty() {
            return;
        }
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.empty_bind, &[]);
        for (tile, slot) in &self.tile_draws {
            let Some(tile) = tile.upgrade() else {
                continue;
            };
            let Some(bind) = tile.bind(nearest) else {
                continue;
            };
            render_pass.set_bind_group(1, bind, &[*slot * UNIFORM_STRIDE as u32]);
            render_pass.draw(0..6, 0..1);
        }
    }
}

const IMAGE_SPV: &[u8] = include_bytes!("image.spv");
