//! Persistent GPU state for the still-image surface: the render pipeline, a
//! small most-recently-used cache of per-image RGBA textures (so navigating
//! back is instant and a long session does not grow VRAM unbounded), the
//! samplers, and the per-draw uniform.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use iced::advanced::image::Id;
use iced::wgpu;
use iced::widget::image::Handle;
use iced::widget::shader;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const UNIFORM_SIZE: u64 = 48;

/// Images up to this size upload inline on the render thread; larger ones go
/// only to the upload thread. Matches iced's MAX_SYNC_SIZE.
const MAX_SYNC_SIZE: usize = 2 * 1024 * 1024;

/// Hands decoded images to the single dedicated upload thread (set up on the
/// pipeline's first build, the only place wgpu gives us the device). One thread
/// keeps uploads serialized, so concurrent 64 MB writes never contend and the
/// tokio pool stays free for decoding.
struct UploadContext {
    jobs: UnboundedSender<Job>,
    max_dim: u32,
}

/// Work for the upload thread. Upload creates a texture off the render thread;
/// Drop frees one off the render thread too, since a 64 MB VRAM free on the
/// render thread stalls the frame (iced drops on its worker for the same reason).
enum Job {
    Upload {
        handle: Handle,
        /// Resolved with the keepalive token once the texture is resident; the
        /// CachedImage holds it to keep the texture from being evicted.
        ready: tokio::sync::oneshot::Sender<Arc<()>>,
    },
    Drop(GpuImage),
}

static UPLOAD_CONTEXT: OnceLock<UploadContext> = OnceLock::new();

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
    /// Resident textures, each paired with a weak ref to its CachedImage's
    /// keepalive. A texture lives as long as its image is cached or displayed,
    /// exactly like iced's Allocation; there is no separate LRU.
    textures: HashMap<Id, (GpuImage, Weak<()>)>,
    current: Option<Id>,
    /// Textures uploaded off the render thread arrive here, drained in prepare.
    /// Behind a mutex only to satisfy the pipeline's Sync bound, since prepare
    /// is the sole drainer and never contends.
    receiver: std::sync::Mutex<UnboundedReceiver<(Id, GpuImage, Weak<()>)>>,
}

struct GpuImage {
    bind_linear: wgpu::BindGroup,
    bind_nearest: wgpu::BindGroup,
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

        let sampler_linear = device.create_sampler(&sampler_desc(wgpu::FilterMode::Linear));
        let sampler_nearest = device.create_sampler(&sampler_desc(wgpu::FilterMode::Nearest));

        // Capture the device here (the only place wgpu hands it to us) and spawn
        // the dedicated upload thread. It owns the cloned device/queue, drains
        // jobs serially, and feeds finished textures to prepare via `receiver`.
        let (results, receiver) = unbounded_channel();
        let (jobs, jobs_rx) = unbounded_channel();
        spawn_upload_thread(
            device.clone(),
            queue.clone(),
            layout.clone(),
            uniforms.clone(),
            sampler_linear.clone(),
            sampler_nearest.clone(),
            jobs_rx,
            results,
        );
        let _ = UPLOAD_CONTEXT.set(UploadContext {
            jobs,
            max_dim: device.limits().max_texture_dimension_2d,
        });

        Self {
            pipeline,
            layout,
            empty_bind,
            uniforms,
            sampler_linear,
            sampler_nearest,
            is_srgb: format.is_srgb(),
            textures: HashMap::new(),
            current: None,
            receiver: std::sync::Mutex::new(receiver),
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
        // Fold in any textures uploaded off the render thread (collect first so
        // the lock is released before touching the cache).
        let drained: Vec<(Id, GpuImage, Weak<()>)> = {
            let mut rx = self.receiver.lock().expect("image upload receiver");
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        };
        for (id, image, keep) in drained {
            self.textures.insert(id, (image, keep));
        }

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
            // A large texture is never uploaded here (that write is what stalled
            // navigation): keep the previous image and wait for the worker.
            // Small ones upload inline, like iced.
            let bytes = (*width as usize) * (*height as usize) * 4;
            if bytes > MAX_SYNC_SIZE {
                return;
            }
            let image = self.upload(device, queue, *width, *height, pixels.as_ref());
            self.textures.insert(id, (image, Weak::new()));
        }
        self.current = Some(id);

        // Free textures whose CachedImage is gone (keepalive expired), keeping
        // the one on screen. The GPU free runs on the upload thread, never the
        // render thread: a 64 MB free here would stall the frame (this is what
        // iced does via worker.drop).
        let expired: Vec<Id> = self
            .textures
            .iter()
            .filter(|(k, (_, keep))| keep.strong_count() == 0 && **k != id)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            if let Some((image, _)) = self.textures.remove(&k) {
                match UPLOAD_CONTEXT.get() {
                    Some(ctx) => {
                        let _ = ctx.jobs.send(Job::Drop(image));
                    }
                    None => drop(image),
                }
            }
        }

        queue.write_buffer(&self.uniforms, 0, &build_uniforms(dst, src, self.is_srgb));
    }

    /// On-render-thread fallback upload, for an image not pre-uploaded off-thread.
    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> GpuImage {
        make_image(
            device,
            queue,
            &self.layout,
            &self.uniforms,
            &self.sampler_linear,
            &self.sampler_nearest,
            width,
            height,
            pixels,
        )
    }

    pub(super) fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>, nearest: bool) {
        let Some((image, _)) = self.current.and_then(|id| self.textures.get(&id)) else {
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

/// Create an empty RGBA texture sized `width` x `height`.
fn create_rgba_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
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
    })
}

/// Build a texture's two bind groups (linear and nearest sampling). The bind
/// groups keep the texture and its view alive, so neither is stored separately.
fn bind_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler_linear: &wgpu::Sampler,
    sampler_nearest: &wgpu::Sampler,
    texture: &wgpu::Texture,
) -> GpuImage {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind = |sampler: &wgpu::Sampler| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scryglass image bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
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
        bind_linear: bind(sampler_linear),
        bind_nearest: bind(sampler_nearest),
    }
}

/// Build a texture and its bind groups via a direct `write_texture`. Used only
/// by the small-image fallback; the worker uses the staging belt instead.
#[allow(clippy::too_many_arguments)]
fn make_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler_linear: &wgpu::Sampler,
    sampler_nearest: &wgpu::Sampler,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> GpuImage {
    let texture = create_rgba_texture(device, width, height);
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
    bind_texture(device, layout, uniforms, sampler_linear, sampler_nearest, &texture)
}

/// Queue an image for the upload thread. Returns false when the pipeline is not
/// built yet (the first image) or the image is oversize for the device, in which
/// case `ready` is dropped and prepare's on-thread fallback covers the display.
pub fn submit_upload(handle: Handle, ready: tokio::sync::oneshot::Sender<Arc<()>>) -> bool {
    let Some(ctx) = UPLOAD_CONTEXT.get() else {
        return false;
    };
    let Handle::Rgba { width, height, .. } = &handle else {
        return false;
    };
    if *width == 0 || *height == 0 || *width > ctx.max_dim || *height > ctx.max_dim {
        return false;
    }
    ctx.jobs.send(Job::Upload { handle, ready }).is_ok()
}

/// The dedicated upload thread, modeled on iced's image worker. It drains jobs
/// one at a time and, after each upload, waits for the GPU on this thread so
/// only one upload is ever in flight (back-pressure). The wait is off the render
/// thread, so it never stalls a frame. Texture frees (Job::Drop) also run here.
#[allow(clippy::too_many_arguments)]
fn spawn_upload_thread(
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    mut jobs: UnboundedReceiver<Job>,
    results: UnboundedSender<(Id, GpuImage, Weak<()>)>,
) {
    std::thread::Builder::new()
        .name("scryglass-image-upload".into())
        .spawn(move || {
            // Upload through a recycled staging belt and copy_buffer_to_texture,
            // like iced's worker, so GPU staging is reused rather than allocated
            // per image. `staging` is the reused copy-source buffer.
            let mut belt = wgpu::util::StagingBelt::new(4 * 1024 * 1024);
            let mut staging: Option<wgpu::Buffer> = None;
            let mut staging_cap: u64 = 0;
            while let Some(job) = jobs.blocking_recv() {
                match job {
                    Job::Upload { handle, ready } => {
                        let Handle::Rgba {
                            id,
                            width,
                            height,
                            pixels,
                        } = &handle
                        else {
                            let _ = ready.send(Arc::new(()));
                            continue;
                        };
                        let (id, width, height) = (*id, *width, *height);
                        let bytes_per_row =
                            (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
                        let total = bytes_per_row as u64 * height as u64;
                        // TODO: free when idle to release the VRAM
                        if staging_cap < total {
                            staging = Some(device.create_buffer(&wgpu::BufferDescriptor {
                                label: Some("scryglass image staging"),
                                size: total,
                                usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                                mapped_at_creation: false,
                            }));
                            staging_cap = total;
                        }
                        let staging_buf = staging.as_ref().expect("staging buffer");
                        let texture = create_rgba_texture(&device, width, height);
                        let mut encoder = device.create_command_encoder(
                            &wgpu::CommandEncoderDescriptor {
                                label: Some("scryglass image upload"),
                            },
                        );
                        // Copy each row into recycled staging at the aligned row
                        // stride, then schedule the buffer-to-texture copy.
                        if let Some(size) = wgpu::BufferSize::new(total) {
                            let mut view =
                                belt.write_buffer(&mut encoder, staging_buf, 0, size, &device);
                            let row = (width * 4) as usize;
                            let stride = bytes_per_row as usize;
                            for y in 0..height as usize {
                                let src = y * row;
                                let dst = y * stride;
                                view[dst..dst + row].copy_from_slice(&pixels[src..src + row]);
                            }
                        }
                        belt.finish();
                        encoder.copy_buffer_to_texture(
                            wgpu::TexelCopyBufferInfo {
                                buffer: staging_buf,
                                layout: wgpu::TexelCopyBufferLayout {
                                    offset: 0,
                                    bytes_per_row: Some(bytes_per_row),
                                    rows_per_image: Some(height),
                                },
                            },
                            wgpu::TexelCopyTextureInfo {
                                texture: &texture,
                                mip_level: 0,
                                origin: wgpu::Origin3d::ZERO,
                                aspect: wgpu::TextureAspect::All,
                            },
                            wgpu::Extent3d {
                                width,
                                height,
                                depth_or_array_layers: 1,
                            },
                        );
                        let submission = queue.submit([encoder.finish()]);
                        belt.recall();
                        let image = bind_texture(
                            &device,
                            &layout,
                            &uniforms,
                            &sampler_linear,
                            &sampler_nearest,
                            &texture,
                        );
                        // Wait for the GPU here so only one upload is ever in
                        // flight (iced's back-pressure), off the render thread.
                        let _ = device.poll(wgpu::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: None,
                        });
                        // The app holds this Arc (keeping the texture resident);
                        // the cache holds the Weak and drops it once it expires.
                        let keep = Arc::new(());
                        let _ = results.send((id, image, Arc::downgrade(&keep)));
                        let _ = ready.send(keep);
                    }
                    Job::Drop(image) => drop(image),
                }
            }
        })
        .expect("spawn scryglass image upload thread");
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
