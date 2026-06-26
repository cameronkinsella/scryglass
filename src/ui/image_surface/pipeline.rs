//! Persistent GPU state for the still-image surface: the render pipeline, a
//! small most-recently-used cache of per-image RGBA textures (so navigating
//! back is instant and a long session does not grow VRAM unbounded), the
//! samplers, and the per-draw uniform.

use std::collections::HashMap;

use iced::advanced::image::Id;
use iced::wgpu;
use iced::widget::image::Handle;
use iced::widget::shader;

/// Resident images kept for instant redisplay: the shown image plus a few
/// recent neighbors. Eviction is least-recently-used.
const MAX_TEXTURES: usize = 8;

const UNIFORM_SIZE: u64 = 48;

/// Persistent GPU state shared by every still-image draw.
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// Bound at slot 0; rust-gpu reserves set 0, so the real bindings are set 1.
    empty_bind: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    is_srgb: bool,
    textures: HashMap<Id, GpuImage>,
    /// LRU order, least-recent first.
    order: Vec<Id>,
    current: Option<Id>,
}

struct GpuImage {
    bind_linear: wgpu::BindGroup,
    bind_nearest: wgpu::BindGroup,
}

impl shader::Pipeline for ImagePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
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
                        has_dynamic_offset: false,
                        min_binding_size: None,
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

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scryglass image uniforms"),
            size: UNIFORM_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            layout,
            empty_bind,
            uniforms,
            sampler_linear: device.create_sampler(&sampler_desc(wgpu::FilterMode::Linear)),
            sampler_nearest: device.create_sampler(&sampler_desc(wgpu::FilterMode::Nearest)),
            is_srgb: format.is_srgb(),
            textures: HashMap::new(),
            order: Vec::new(),
            current: None,
        }
    }
}

impl ImagePipeline {
    pub(super) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        handle: &Handle,
        dst: [f32; 4],
        src: [f32; 4],
    ) {
        // The cache only ever holds Rgba handles (the load pipeline decodes to
        // raw pixels), so anything else has nothing to upload.
        let Handle::Rgba {
            id,
            width,
            height,
            pixels,
        } = handle
        else {
            return;
        };
        let id = *id;
        if !self.textures.contains_key(&id) {
            let image = self.upload(device, queue, *width, *height, pixels.as_ref());
            self.textures.insert(id, image);
        }

        // Most-recently-used to the back, then evict the oldest over the cap.
        self.order.retain(|k| *k != id);
        self.order.push(id);
        while self.textures.len() > MAX_TEXTURES && self.order.len() > 1 {
            let old = self.order.remove(0);
            self.textures.remove(&old);
        }
        self.current = Some(id);

        queue.write_buffer(&self.uniforms, 0, &build_uniforms(dst, src, self.is_srgb));
    }

    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> GpuImage {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("scryglass image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        // The bind groups keep the texture and its view alive, so there is no
        // need to store the texture itself.
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = |sampler: &wgpu::Sampler| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scryglass image bind group"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        };
        GpuImage {
            bind_linear: bind(&self.sampler_linear),
            bind_nearest: bind(&self.sampler_nearest),
        }
    }

    pub(super) fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>, nearest: bool) {
        let Some(image) = self.current.and_then(|id| self.textures.get(&id)) else {
            return;
        };
        let bind = if nearest {
            &image.bind_nearest
        } else {
            &image.bind_linear
        };
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.empty_bind, &[]);
        render_pass.set_bind_group(1, bind, &[]);
        render_pass.draw(0..6, 0..1);
    }
}

fn sampler_desc(filter: wgpu::FilterMode) -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        label: Some("scryglass image sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: filter,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    }
}

/// Pack the per-draw uniform block: the destination and source rects, plus the
/// sRGB-target flag. Layout matches the shader `Uniforms` struct (48 bytes).
fn build_uniforms(dst: [f32; 4], src: [f32; 4], is_srgb: bool) -> [u8; 48] {
    let mut buf = [0u8; 48];
    let floats = [
        dst[0], dst[1], dst[2], dst[3], src[0], src[1], src[2], src[3],
    ];
    for (i, f) in floats.iter().enumerate() {
        buf[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf[32..36].copy_from_slice(&(is_srgb as u32).to_le_bytes());
    buf
}

const IMAGE_SPV: &[u8] = include_bytes!("image.spv");
