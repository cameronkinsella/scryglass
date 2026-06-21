//! Persistent GPU state for the video surface: the render pipeline, the
//! plane textures (reused across frames, recreated only on a resize), the
//! samplers, and the per-frame uniform buffer. The YUV-to-RGB conversion
//! runs in the shader, so playback never pays for a CPU color conversion.

use iced::wgpu;
use iced::widget::shader;

use crate::video::{VideoFrame, YuvFormat, YuvMatrix, YuvRange};

/// Persistent GPU state shared by every video frame.
pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// Bound at slot 0; the real bindings are at set 1.
    empty_bind: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    is_srgb: bool,
    textures: Option<YuvTextures>,
    last_id: Option<u64>,
}

struct YuvTextures {
    width: u32,
    height: u32,
    chroma_width: u32,
    chroma_height: u32,
    format: YuvFormat,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_linear: wgpu::BindGroup,
    bind_nearest: wgpu::BindGroup,
}

const UNIFORM_SIZE: u64 = 48;

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scryglass video yuv"),
            source: wgpu::ShaderSource::SpirV(wgpu::util::make_spirv_raw(YUV_SPV)),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scryglass video bind group layout"),
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
                plane_entry(1),
                plane_entry(2),
                plane_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Empty layout for set 0 (rust-gpu reserves it).
        let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scryglass video empty set"),
            entries: &[],
        });
        let empty_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scryglass video empty set"),
            layout: &empty_layout,
            entries: &[],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scryglass video pipeline layout"),
            bind_group_layouts: &[&empty_layout, &layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scryglass video pipeline"),
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
                    blend: Some(wgpu::BlendState::REPLACE),
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
            label: Some("scryglass video uniforms"),
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
            textures: None,
            last_id: None,
        }
    }
}

impl VideoPipeline {
    pub(super) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &VideoFrame,
        dst: [f32; 4],
        src: [f32; 4],
    ) {
        let stale = match &self.textures {
            Some(t) => {
                t.width != frame.width
                    || t.height != frame.height
                    || t.chroma_width != frame.chroma_width
                    || t.chroma_height != frame.chroma_height
                    || t.format != frame.format
            }
            None => true,
        };
        if stale {
            let textures = self.create_textures(device, frame);
            self.textures = Some(textures);
            self.last_id = None;
        }

        let textures = self.textures.as_ref().expect("textures just created");
        if self.last_id != Some(frame.id) {
            write_plane(
                queue,
                &textures.y,
                frame.width,
                frame.height,
                frame.width,
                &frame.y,
            );
            match frame.format {
                YuvFormat::I420 => {
                    write_plane(
                        queue,
                        &textures.u,
                        frame.chroma_width,
                        frame.chroma_height,
                        frame.chroma_width,
                        &frame.u,
                    );
                    write_plane(
                        queue,
                        &textures.v,
                        frame.chroma_width,
                        frame.chroma_height,
                        frame.chroma_width,
                        &frame.v,
                    );
                }
                // Interleaved UV in one Rg8 texture: two bytes per sample.
                YuvFormat::Nv12 => {
                    write_plane(
                        queue,
                        &textures.u,
                        frame.chroma_width,
                        frame.chroma_height,
                        frame.chroma_width * 2,
                        &frame.u,
                    );
                }
            }
            self.last_id = Some(frame.id);
        }

        queue.write_buffer(
            &self.uniforms,
            0,
            &build_uniforms(dst, src, frame, self.is_srgb),
        );
    }

    fn create_textures(&self, device: &wgpu::Device, frame: &VideoFrame) -> YuvTextures {
        let r8 = wgpu::TextureFormat::R8Unorm;
        let y = plane_texture(device, frame.width, frame.height, r8, "scryglass video y");
        let (u, v) = match frame.format {
            YuvFormat::I420 => (
                plane_texture(
                    device,
                    frame.chroma_width,
                    frame.chroma_height,
                    r8,
                    "scryglass video u",
                ),
                plane_texture(
                    device,
                    frame.chroma_width,
                    frame.chroma_height,
                    r8,
                    "scryglass video v",
                ),
            ),
            // NV12: interleaved UV in one Rg8 texture; v is an unused stub.
            YuvFormat::Nv12 => (
                plane_texture(
                    device,
                    frame.chroma_width,
                    frame.chroma_height,
                    wgpu::TextureFormat::Rg8Unorm,
                    "scryglass video uv",
                ),
                plane_texture(device, 1, 1, r8, "scryglass video v unused"),
            ),
        };
        let yv = y.create_view(&wgpu::TextureViewDescriptor::default());
        let uv = u.create_view(&wgpu::TextureViewDescriptor::default());
        let vv = v.create_view(&wgpu::TextureViewDescriptor::default());

        let bind = |sampler: &wgpu::Sampler| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scryglass video bind group"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&yv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&uv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&vv),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        };

        YuvTextures {
            width: frame.width,
            height: frame.height,
            chroma_width: frame.chroma_width,
            chroma_height: frame.chroma_height,
            format: frame.format,
            bind_linear: bind(&self.sampler_linear),
            bind_nearest: bind(&self.sampler_nearest),
            y,
            u,
            v,
        }
    }

    pub(super) fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>, nearest: bool) {
        let Some(textures) = &self.textures else {
            return;
        };
        let bind = if nearest {
            &textures.bind_nearest
        } else {
            &textures.bind_linear
        };
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.empty_bind, &[]);
        render_pass.set_bind_group(1, bind, &[]);
        render_pass.draw(0..6, 0..1);
    }
}

fn plane_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn plane_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    bytes_per_row: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn sampler_desc(filter: wgpu::FilterMode) -> wgpu::SamplerDescriptor<'static> {
    wgpu::SamplerDescriptor {
        label: Some("scryglass video sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: filter,
        mipmap_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    }
}

/// Pack the per-frame uniform block: geometry rects plus color parameters.
/// Layout matches the WGSL `Uniforms` struct exactly (48 bytes).
fn build_uniforms(dst: [f32; 4], src: [f32; 4], frame: &VideoFrame, is_srgb: bool) -> [u8; 48] {
    let mut buf = [0u8; 48];
    let floats = [
        dst[0], dst[1], dst[2], dst[3], src[0], src[1], src[2], src[3],
    ];
    for (i, f) in floats.iter().enumerate() {
        buf[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let matrix: u32 = match frame.matrix {
        YuvMatrix::Bt709 => 1,
        YuvMatrix::Bt601 => 0,
    };
    let full: u32 = match frame.range {
        YuvRange::Full => 1,
        YuvRange::Limited => 0,
    };
    let format: u32 = match frame.format {
        YuvFormat::Nv12 => 1,
        YuvFormat::I420 => 0,
    };
    buf[32..36].copy_from_slice(&matrix.to_le_bytes());
    buf[36..40].copy_from_slice(&full.to_le_bytes());
    buf[40..44].copy_from_slice(&(is_srgb as u32).to_le_bytes());
    buf[44..48].copy_from_slice(&format.to_le_bytes());
    buf
}

const YUV_SPV: &[u8] = include_bytes!("yuv.spv");

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn frame(matrix: YuvMatrix, range: YuvRange, format: YuvFormat) -> VideoFrame {
        VideoFrame {
            id: 0,
            width: 2,
            height: 2,
            chroma_width: 1,
            chroma_height: 1,
            format,
            y: vec![],
            u: vec![],
            v: vec![],
            matrix,
            range,
            timestamp: Duration::ZERO,
        }
    }

    fn u32_at(buf: &[u8; 48], offset: usize) -> u32 {
        u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
    }

    fn f32_at(buf: &[u8; 48], offset: usize) -> f32 {
        f32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn packs_dst_then_src_floats_then_flags() {
        let dst = [0.1, 0.2, 0.3, 0.4];
        let src = [0.5, 0.6, 0.7, 0.8];
        let buf = build_uniforms(
            dst,
            src,
            &frame(YuvMatrix::Bt709, YuvRange::Full, YuvFormat::Nv12),
            true,
        );
        for (i, v) in dst.iter().chain(src.iter()).enumerate() {
            assert_eq!(f32_at(&buf, i * 4), *v);
        }
        assert_eq!(u32_at(&buf, 32), 1, "bt709");
        assert_eq!(u32_at(&buf, 36), 1, "full range");
        assert_eq!(u32_at(&buf, 40), 1, "srgb target");
        assert_eq!(u32_at(&buf, 44), 1, "nv12");
    }

    #[test]
    fn bt601_limited_i420_on_linear_target_are_all_zero_flags() {
        let buf = build_uniforms(
            [0.0; 4],
            [0.0; 4],
            &frame(YuvMatrix::Bt601, YuvRange::Limited, YuvFormat::I420),
            false,
        );
        assert_eq!(u32_at(&buf, 32), 0);
        assert_eq!(u32_at(&buf, 36), 0);
        assert_eq!(u32_at(&buf, 40), 0);
        assert_eq!(u32_at(&buf, 44), 0);
    }
}
