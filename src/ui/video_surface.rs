//! GPU video surface: uploads decoded YUV planes and converts them to RGB
//! in a shader. Playback never pays for a CPU color conversion or a
//! per-frame upload of full RGBA, and the planes are 1.5 bytes per pixel
//! instead of 4. Zoom, pan, and fit reuse the still-image display math, so
//! video and stills share one geometry and never diverge.

use std::sync::Arc;

use iced::widget::shader;
use iced::{Element, Length, Rectangle, mouse, wgpu};

use crate::app::Message;
use crate::ui::image_display::{DisplayMath, display_math};
use crate::video::{VideoFrame, YuvMatrix, YuvRange};

/// Build the video surface element for the current frame at the given
/// zoom/pan. Fills the image area like the still-image widget does.
pub fn view(
    frame: Arc<VideoFrame>,
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    pixelated: bool,
) -> Element<'static, Message> {
    shader::Shader::new(VideoSurface::new(frame, zoom, pan, viewport, pixelated))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The shader program: holds the frame to show and where to put it.
struct VideoSurface {
    frame: Arc<VideoFrame>,
    valid: bool,
    /// Destination rect in normalized widget space: x0, y0, x1, y1.
    dst: [f32; 4],
    /// Source rect in texture UV space: u0, v0, u1, v1.
    src: [f32; 4],
    /// Nearest sampling when zoomed past 100% with crisp pixels on.
    nearest: bool,
}

impl VideoSurface {
    fn new(
        frame: Arc<VideoFrame>,
        zoom: f32,
        pan: (f32, f32),
        viewport: (f32, f32),
        pixelated: bool,
    ) -> Self {
        let original = (frame.width, frame.height);
        let nearest = pixelated && zoom > 1.0;
        match geometry(zoom, pan, viewport, original) {
            Some((dst, src)) => Self {
                frame,
                valid: true,
                dst,
                src,
                nearest,
            },
            None => Self {
                frame,
                valid: false,
                dst: [0.0; 4],
                src: [0.0; 4],
                nearest,
            },
        }
    }
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
            valid: self.valid,
            dst: self.dst,
            src: self.src,
            nearest: self.nearest,
        }
    }
}

/// Convert the still-image display math into a destination rect (normalized
/// widget space) and a source rect (texture UV). Video textures are native
/// resolution, so texture space equals original space.
fn geometry(
    zoom: f32,
    pan: (f32, f32),
    viewport: (f32, f32),
    original: (u32, u32),
) -> Option<([f32; 4], [f32; 4])> {
    let (vw, vh) = viewport;
    let (tw, th) = (original.0 as f32, original.1 as f32);
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }

    // Centered destination rect for a shown size in logical pixels.
    let centered = |shown_w: f32, shown_h: f32| {
        let x0 = (vw - shown_w) / 2.0 / vw;
        let y0 = (vh - shown_h) / 2.0 / vh;
        [x0, y0, x0 + shown_w / vw, y0 + shown_h / vh]
    };

    match display_math(zoom, pan, viewport, original, original) {
        DisplayMath::Empty => None,
        DisplayMath::Fit { scale_factor } => {
            let contain = (vw / tw).min(vh / th);
            let dst = centered(tw * contain * scale_factor, th * contain * scale_factor);
            Some((dst, [0.0, 0.0, 1.0, 1.0]))
        }
        DisplayMath::Crop { rect } => {
            let (rw, rh) = (rect.width as f32, rect.height as f32);
            let contain = (vw / rw).min(vh / rh);
            let dst = centered(rw * contain, rh * contain);
            let src = [
                rect.x as f32 / tw,
                rect.y as f32 / th,
                (rect.x as f32 + rw) / tw,
                (rect.y as f32 + rh) / th,
            ];
            Some((dst, src))
        }
    }
}

/// A single frame's worth of work handed to the renderer.
pub struct VideoPrimitive {
    frame: Arc<VideoFrame>,
    valid: bool,
    dst: [f32; 4],
    src: [f32; 4],
    nearest: bool,
}

impl std::fmt::Debug for VideoPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPrimitive")
            .field("frame_id", &self.frame.id)
            .field("valid", &self.valid)
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
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if self.valid {
            pipeline.prepare(device, queue, &self.frame, self.dst, self.src);
        }
    }

    fn draw(&self, pipeline: &VideoPipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        if self.valid {
            pipeline.draw(render_pass, self.nearest);
        }
        true
    }
}

/// Persistent GPU state shared by every video frame: the pipeline, the
/// plane textures (reused across frames, recreated only on a resize), and
/// the uniform buffer.
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
    fn prepare(
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
            write_plane(queue, &textures.y, frame.width, frame.height, &frame.y);
            write_plane(
                queue,
                &textures.u,
                frame.chroma_width,
                frame.chroma_height,
                &frame.u,
            );
            write_plane(
                queue,
                &textures.v,
                frame.chroma_width,
                frame.chroma_height,
                &frame.v,
            );
            self.last_id = Some(frame.id);
        }

        queue.write_buffer(
            &self.uniforms,
            0,
            &build_uniforms(dst, src, frame, self.is_srgb),
        );
    }

    fn create_textures(&self, device: &wgpu::Device, frame: &VideoFrame) -> YuvTextures {
        let y = plane_texture(device, frame.width, frame.height, "scryglass video y");
        let u = plane_texture(
            device,
            frame.chroma_width,
            frame.chroma_height,
            "scryglass video u",
        );
        let v = plane_texture(
            device,
            frame.chroma_width,
            frame.chroma_height,
            "scryglass video v",
        );
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
            bind_linear: bind(&self.sampler_linear),
            bind_nearest: bind(&self.sampler_nearest),
            y,
            u,
            v,
        }
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>, nearest: bool) {
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

fn plane_texture(device: &wgpu::Device, width: u32, height: u32, label: &str) -> wgpu::Texture {
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
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(queue: &wgpu::Queue, texture: &wgpu::Texture, width: u32, height: u32, data: &[u8]) {
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
            bytes_per_row: Some(width),
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
    buf[32..36].copy_from_slice(&matrix.to_le_bytes());
    buf[36..40].copy_from_slice(&full.to_le_bytes());
    buf[40..44].copy_from_slice(&(is_srgb as u32).to_le_bytes());
    buf
}

const YUV_SPV: &[u8] = include_bytes!("video_surface/yuv.spv");
