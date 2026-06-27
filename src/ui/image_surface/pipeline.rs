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
        /// The image id this texture is for. View-res and full-res uploads share
        /// one id (the `handle` carries the pixels), so promoting or demoting an
        /// image just replaces its texture.
        id: Id,
        handle: Handle,
        /// Resolved with the keepalive once the texture is resident. The app
        /// holds it for as long as it wants the image; dropping it frees the
        /// texture at once.
        ready: tokio::sync::oneshot::Sender<Keepalive>,
    },
    Drop(GpuImage),
}

static UPLOAD_CONTEXT: OnceLock<UploadContext> = OnceLock::new();

/// A GPU-resident image, owned by the app through its [`Keepalive`]. Dropping
/// the last reference frees the texture off the render thread immediately, so a
/// minimized or closed window reclaims its VRAM at once rather than waiting for
/// some later frame to sweep it.
pub struct ResidentImage {
    image: Option<GpuImage>,
    drop_tx: Option<UnboundedSender<Job>>,
}

impl std::fmt::Debug for ResidentImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResidentImage")
            .field("resident", &self.image.is_some())
            .finish()
    }
}

impl Drop for ResidentImage {
    fn drop(&mut self) {
        if let Some(image) = self.image.take() {
            match &self.drop_tx {
                // Free on the upload thread, never the render thread.
                Some(tx) => {
                    let _ = tx.send(Job::Drop(image));
                }
                None => drop(image),
            }
        }
    }
}

/// The app-held handle that keeps an uploaded image resident. Cheap to clone
/// (a refcount bump); the texture lives until the last clone drops.
pub type Keepalive = Arc<ResidentImage>;

/// A keepalive with no texture, for tests that only need its refcount token.
#[cfg(test)]
pub fn test_keepalive() -> Keepalive {
    Arc::new(ResidentImage {
        image: None,
        drop_tx: None,
    })
}

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
    /// Weak refs to app-owned resident images. The app's keepalive owns the
    /// texture, so a window dropping its keepalive frees the VRAM at once; the
    /// pipeline only borrows, drawing while the weak still upgrades.
    textures: HashMap<Id, Weak<ResidentImage>>,
    /// Pipeline-owned fallbacks: small images uploaded inline, or the first
    /// image before the upload thread exists, which have no app keepalive. Held
    /// to at most the current image, since they are tiny and transient.
    owned: HashMap<Id, Arc<ResidentImage>>,
    current: Option<Id>,
    /// Textures uploaded off the render thread arrive here, drained in prepare.
    /// Behind a mutex only to satisfy the pipeline's Sync bound, since prepare
    /// is the sole drainer and never contends.
    receiver: std::sync::Mutex<UnboundedReceiver<(Id, Weak<ResidentImage>)>>,
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
            jobs.clone(),
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
            owned: HashMap::new(),
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
        // the lock is released before touching the maps), and drop weak refs to
        // images the app has since released.
        let drained: Vec<(Id, Weak<ResidentImage>)> = {
            let mut rx = self.receiver.lock().expect("image upload receiver");
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        };
        for (id, weak) in drained {
            self.textures.insert(id, weak);
            // A worker upload supersedes any inline fallback for the same image.
            self.owned.remove(&id);
        }
        self.textures.retain(|_, weak| weak.strong_count() > 0);

        // The cache only ever holds Rgba handles (the load pipeline decodes to
        // raw pixels), so anything else has nothing to upload.
        let Handle::Rgba {
            id,
            width,
            height,
            pixels,
        } = handle
        else {
            self.current = None;
            return;
        };
        let id = *id;
        let resident = self.owned.contains_key(&id)
            || self.textures.get(&id).is_some_and(|w| w.strong_count() > 0);
        if !resident {
            // A large texture is never uploaded here (that write is what stalled
            // navigation): draw nothing this frame and wait for the worker, so a
            // not-yet-resident window never borrows another window's texture.
            // Small ones upload inline, like iced, and the pipeline owns them.
            let bytes = (*width as usize) * (*height as usize) * 4;
            if bytes > MAX_SYNC_SIZE {
                self.current = None;
                return;
            }
            let image = self.upload(device, queue, *width, *height, pixels.as_ref());
            let drop_tx = UPLOAD_CONTEXT.get().map(|ctx| ctx.jobs.clone());
            self.owned.insert(
                id,
                Arc::new(ResidentImage {
                    image: Some(image),
                    drop_tx,
                }),
            );
        }
        self.current = Some(id);
        // Inline fallbacks are tiny and transient; keep only the current one.
        self.owned.retain(|k, _| *k == id);

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
        let Some(id) = self.current else {
            return;
        };
        // A pipeline-owned fallback, else the app-owned texture while its
        // keepalive still upgrades. The upgraded Arc must outlive the draw call.
        if let Some(resident) = self.owned.get(&id) {
            self.draw_resident(render_pass, resident, nearest);
        } else if let Some(resident) = self.textures.get(&id).and_then(Weak::upgrade) {
            self.draw_resident(render_pass, &resident, nearest);
        }
    }

    fn draw_resident(
        &self,
        render_pass: &mut wgpu::RenderPass<'_>,
        resident: &ResidentImage,
        nearest: bool,
    ) {
        let Some(image) = resident.image.as_ref() else {
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
    bind_texture(
        device,
        layout,
        uniforms,
        sampler_linear,
        sampler_nearest,
        &texture,
    )
}

/// Queue an image for the upload thread at its full resolution. Returns false
/// when the pipeline is not built yet (the first image) or the image is oversize
/// for the device, in which case `ready` is dropped and prepare's on-thread
/// fallback covers the display.
pub fn submit_upload(handle: Handle, ready: tokio::sync::oneshot::Sender<Keepalive>) -> bool {
    submit_upload_at(handle.id(), handle, ready)
}

/// Queue `handle`'s pixels as the texture for image `id`, which may differ from
/// `handle`'s own id. A view-res and a full-res upload for the same image share
/// one `id`, so promoting or demoting an image is just another upload that
/// replaces its texture.
pub fn submit_upload_at(
    id: Id,
    handle: Handle,
    ready: tokio::sync::oneshot::Sender<Keepalive>,
) -> bool {
    let Some(ctx) = UPLOAD_CONTEXT.get() else {
        return false;
    };
    let Handle::Rgba { width, height, .. } = &handle else {
        return false;
    };
    if *width == 0 || *height == 0 || *width > ctx.max_dim || *height > ctx.max_dim {
        return false;
    }
    ctx.jobs.send(Job::Upload { id, handle, ready }).is_ok()
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
    drop_tx: UnboundedSender<Job>,
    results: UnboundedSender<(Id, Weak<ResidentImage>)>,
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
                    Job::Upload { id, handle, ready } => {
                        let Handle::Rgba {
                            width,
                            height,
                            pixels,
                            ..
                        } = &handle
                        else {
                            // Never reached (loads decode to Rgba); drop `ready`
                            // so the awaiter sees no keepalive.
                            continue;
                        };
                        let (width, height) = (*width, *height);
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
                        let mut encoder =
                            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("scryglass image upload"),
                            });
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
                        // the pipeline holds the Weak and draws while it upgrades.
                        // Dropping the last Arc frees the texture via `drop_tx`.
                        let resident = Arc::new(ResidentImage {
                            image: Some(image),
                            drop_tx: Some(drop_tx.clone()),
                        });
                        let _ = results.send((id, Arc::downgrade(&resident)));
                        let _ = ready.send(resident);
                    }
                    Job::Drop(image) => {
                        drop(image);
                        // Dropping a texture only queues it for destruction;
                        // poll so the driver reclaims the VRAM now, even while
                        // every window is minimized and nothing else renders.
                        let _ = device.poll(wgpu::PollType::Poll);
                    }
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
