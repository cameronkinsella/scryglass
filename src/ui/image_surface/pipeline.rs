//! Persistent GPU state for the still-image surface: the render pipeline, the
//! per-draw uniform, and the dedicated upload thread. Textures themselves are
//! app-owned `Keepalive`s drawn directly, not held here.

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use iced::wgpu;
use iced::widget::image::Handle;
use iced::widget::shader;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::config::DownscaleKernel;
use crate::media::tiles::{TileCache, TileKey};

const UNIFORM_SIZE: u64 = 80;

/// Hands decoded images to the single dedicated upload thread (set up on the
/// pipeline's first build, the only place wgpu gives us the device). One thread
/// keeps uploads serialized, so concurrent 64 MB writes never contend and the
/// tokio pool stays free for decoding.
struct UploadContext {
    jobs: UnboundedSender<Job>,
    max_dim: u32,
    /// The kernel the last display draw used, so the off-thread view-res render
    /// downscales with exactly what is on screen. Read by the render job, written
    /// by every draw, so it tracks the live `downscale_kernel` with no plumbing
    /// through the load/prefetch call graph.
    kernel: AtomicU8,
    /// The display scale factor the last draw reported (`f32` bits), so a view-res
    /// copy is sized to the physical display rather than a fixed headroom guess.
    scale_factor: AtomicU32,
}

/// Work for the upload thread. Upload creates a texture off the render thread;
/// Drop frees one off the render thread too, since a 64 MB VRAM free on the
/// render thread stalls the frame (iced drops on its worker for the same reason).
enum Job {
    Upload {
        handle: Handle,
        /// Resolved with the keepalive once the texture is resident. The app
        /// holds it for as long as it wants the image; dropping it frees the
        /// texture at once.
        ready: tokio::sync::oneshot::Sender<Keepalive>,
    },
    /// Downscale a resident texture to `target` on the GPU through the display
    /// shader, so a demoted view-res copy is generated with the very kernel the
    /// full-res view uses and the swap is invisible.
    RenderDownscale {
        source: Keepalive,
        target: (u32, u32),
        ready: tokio::sync::oneshot::Sender<Keepalive>,
    },
    Drop(GpuImage),
}

static UPLOAD_CONTEXT: OnceLock<UploadContext> = OnceLock::new();

/// A GPU-resident image, owned by the app through its [`Keepalive`]. Dropping
/// the last reference frees the texture off the render thread immediately, so a
/// minimized or closed window reclaims its VRAM at once rather than waiting for
/// some later frame to sweep it.
///
/// A still too large for one texture is resident as a [`TileSet`] instead of a
/// single texture; its tiles are small resident images themselves, so upload
/// and off-thread VRAM release work the same way tile by tile.
pub struct ResidentImage {
    image: Option<GpuImage>,
    drop_tx: Option<UnboundedSender<Job>>,
    tiles: Option<TileSet>,
}

impl ResidentImage {
    /// The resident texture's size in texels, or `None` for a tile pyramid or
    /// the tokenless test keepalive. The downscale shader needs it to step
    /// taps by whole texels.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.image.as_ref().map(|image| image.size)
    }

    /// The texture view to sample when rendering this image into another texture
    /// (the view-res downscale), or `None` for the tokenless test keepalive.
    fn input_view(&self) -> Option<&wgpu::TextureView> {
        self.image.as_ref().map(|image| &image.view)
    }

    /// The tile pyramid, when this resident is a tiled still.
    pub fn tiles(&self) -> Option<&TileSet> {
        self.tiles.as_ref()
    }

    /// A fresh, empty tile pyramid for an `original`-sized still. Purely
    /// CPU-side bookkeeping: the VRAM arrives tile by tile as each one is
    /// produced and uploaded like any small image.
    pub fn tiled(original: (u32, u32)) -> Keepalive {
        Arc::new(ResidentImage {
            image: None,
            drop_tx: None,
            tiles: Some(TileSet {
                original,
                tiles: Mutex::new(TileCache::new(MAX_CACHED_TILES)),
            }),
        })
    }
}

/// Most tiles a pyramid keeps resident: a window's worth at the active level
/// plus pan margin, matching ImageGlass 10's cache bound. Older tiles drop
/// (freeing their VRAM) and are re-produced from the RAM source on return.
const MAX_CACHED_TILES: usize = 100;

/// The resident form of a tiled still: its bounded tile cache plus the
/// uncapped source size the tile grid maps. Shared cross-window inside one
/// [`Keepalive`]; tiles stream in through the mutex as they are produced.
pub struct TileSet {
    original: (u32, u32),
    tiles: Mutex<TileCache<Keepalive>>,
}

impl TileSet {
    /// The uncapped source dimensions the pyramid maps.
    #[expect(dead_code, reason = "read by the tile draw loop, landing next")]
    pub fn original(&self) -> (u32, u32) {
        self.original
    }

    /// Install a produced tile, evicting the stalest past the cap.
    pub fn insert(&self, key: TileKey, tile: Keepalive) {
        if let Ok(mut tiles) = self.tiles.lock() {
            tiles.insert(key, tile);
        }
    }

    /// The tile for `key`, freshly marked as used.
    #[expect(dead_code, reason = "read by the tile draw loop, landing next")]
    pub fn get(&self, key: TileKey) -> Option<Keepalive> {
        self.tiles.lock().ok()?.get(key).cloned()
    }

    /// Whether `key` is resident, without touching its recency.
    #[expect(dead_code, reason = "read by the tile demand pass, landing next")]
    pub fn contains(&self, key: TileKey) -> bool {
        self.tiles
            .lock()
            .map(|tiles| tiles.contains(key))
            .unwrap_or(false)
    }
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
        tiles: None,
    })
}

/// Persistent GPU state shared by every still-image draw.
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    /// Bound at slot 0; rust-gpu reserves set 0, so the real bindings are set 1.
    empty_bind: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    is_srgb: bool,
}

struct GpuImage {
    bind_linear: wgpu::BindGroup,
    bind_nearest: wgpu::BindGroup,
    /// Kept so this texture can be sampled as the source of a view-res render.
    view: wgpu::TextureView,
    size: (u32, u32),
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
            size: UNIFORM_SIZE,
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

        // Capture the device here (the only place wgpu hands it to us) and spawn
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
        }
    }
}

impl ImagePipeline {
    /// Write the per-draw uniforms for the resident-texture draw path: the dst/src
    /// rects plus the downscale kernel and the footprint it is scaled by. Also
    /// records the kernel so an off-thread view-res render bakes with the same one.
    pub(super) fn write_uniforms(
        &self,
        queue: &wgpu::Queue,
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
            0,
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

/// Create an empty RGBA texture sized `width` x `height`. `render` adds the
/// render-attachment usage for a view-res downscale target (which is drawn into
/// rather than copied into).
fn create_rgba_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    render: bool,
) -> wgpu::Texture {
    let usage = if render {
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT
    } else {
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST
    };
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
        usage,
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
    // Scope the closure so its borrow of `view` ends before `view` moves into the
    // struct (it is kept there to source a later view-res render).
    let (bind_linear, bind_nearest) = {
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
        (bind(sampler_linear), bind(sampler_nearest))
    };
    GpuImage {
        bind_linear,
        bind_nearest,
        view,
        size: (texture.width(), texture.height()),
    }
}

/// Queue an image for the upload thread. Returns false when the pipeline is not
/// built yet (the first image) or the image is oversize for the device, in which
/// case `ready` is dropped and the display falls back to its thumbnail.
pub fn submit_upload(handle: Handle, ready: tokio::sync::oneshot::Sender<Keepalive>) -> bool {
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

/// The display scale factor the most recent draw reported (`1.0` before any draw),
/// so the view-res sizing can target physical display pixels.
pub fn current_scale_factor() -> f32 {
    UPLOAD_CONTEXT.get().map_or(1.0, |ctx| {
        f32::from_bits(ctx.scale_factor.load(Ordering::Relaxed))
    })
}

/// The downscale kernel the most recent draw used, so a CPU-built view-res copy
/// (a fresh prefetch) is filtered with the very kernel the on-screen full-res is,
/// exactly like the GPU render path a demote takes.
pub fn current_kernel() -> DownscaleKernel {
    UPLOAD_CONTEXT
        .get()
        .map_or_else(DownscaleKernel::default, |ctx| {
            DownscaleKernel::from_u8(ctx.kernel.load(Ordering::Relaxed))
        })
}

/// Queue a resident texture to be downscaled to `target` on the GPU through the
/// display shader, resolving the receiver with the view-res keepalive. Returns
/// `None` (the caller then falls back to a CPU downscale) when the pipeline is not
/// built yet or the channel is closed.
pub fn submit_render_downscale(
    source: Keepalive,
    target: (u32, u32),
) -> Option<tokio::sync::oneshot::Receiver<Keepalive>> {
    let ctx = UPLOAD_CONTEXT.get()?;
    let (ready, rx) = tokio::sync::oneshot::channel();
    ctx.jobs
        .send(Job::RenderDownscale {
            source,
            target,
            ready,
        })
        .ok()?;
    Some(rx)
}

/// The GPU resources the upload thread owns: what it uploads with, plus the second
/// pipeline and uniform it renders view-res copies with.
struct UploadThread {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    uniforms: wgpu::Buffer,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    render_pipeline: wgpu::RenderPipeline,
    render_uniforms: wgpu::Buffer,
    empty_bind: wgpu::BindGroup,
}

/// The dedicated upload thread, modeled on iced's image worker. It drains jobs
/// one at a time and, after each upload, waits for the GPU on this thread so
/// only one upload is ever in flight (back-pressure). The wait is off the render
/// thread, so it never stalls a frame. View-res renders and texture frees
/// (Job::Drop) also run here.
fn spawn_upload_thread(
    t: UploadThread,
    mut jobs: UnboundedReceiver<Job>,
    drop_tx: UnboundedSender<Job>,
) {
    let UploadThread {
        device,
        queue,
        layout,
        uniforms,
        sampler_linear,
        sampler_nearest,
        render_pipeline,
        render_uniforms,
        empty_bind,
    } = t;
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
                        let texture = create_rgba_texture(&device, width, height, false);
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
                        // dropping the last Arc frees the texture via `drop_tx`.
                        let resident = Arc::new(ResidentImage {
                            image: Some(image),
                            drop_tx: Some(drop_tx.clone()),
                            tiles: None,
                        });
                        let _ = ready.send(resident);
                    }
                    Job::RenderDownscale {
                        source,
                        target,
                        ready,
                    } => {
                        // Skip the tokenless test keepalive; the caller then falls
                        // back to its CPU downscale.
                        let (Some(src_view), Some((sw, sh))) = (source.input_view(), source.size())
                        else {
                            continue;
                        };
                        let (tw, th) = (target.0.max(1), target.1.max(1));
                        // Bake with the kernel the last display draw used, so this
                        // copy matches the full-res view it replaces.
                        let kernel = DownscaleKernel::from_u8(
                            UPLOAD_CONTEXT
                                .get()
                                .map_or(DownscaleKernel::default().to_u8(), |c| {
                                    c.kernel.load(Ordering::Relaxed)
                                }),
                        );
                        let (selector, bc) = kernel.shader_params();
                        let footprint = [sw as f32 / tw as f32, sh as f32 / th as f32];
                        // The target is a plain Rgba8Unorm texture (not sRGB), so the
                        // shader is told to write sRGB-encoded values, matching every
                        // uploaded image. is_srgb = false does exactly that.
                        queue.write_buffer(
                            &render_uniforms,
                            0,
                            &build_uniforms(
                                [0.0, 0.0, 1.0, 1.0],
                                [0.0, 0.0, 1.0, 1.0],
                                false,
                                selector,
                                footprint,
                                [sw as f32, sh as f32],
                                bc,
                            ),
                        );
                        let out = create_rgba_texture(&device, tw, th, true);
                        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
                        let in_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("scryglass view-res input"),
                            layout: &layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: render_uniforms.as_entire_binding(),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::TextureView(src_view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 2,
                                    resource: wgpu::BindingResource::Sampler(&sampler_linear),
                                },
                            ],
                        });
                        let mut encoder =
                            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("scryglass view-res render"),
                            });
                        {
                            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("scryglass view-res pass"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &out_view,
                                    depth_slice: None,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                        store: wgpu::StoreOp::Store,
                                    },
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                            });
                            pass.set_pipeline(&render_pipeline);
                            pass.set_bind_group(0, &empty_bind, &[]);
                            pass.set_bind_group(1, &in_bind, &[]);
                            pass.draw(0..6, 0..1);
                        }
                        let submission = queue.submit([encoder.finish()]);
                        let image = bind_texture(
                            &device,
                            &layout,
                            &uniforms,
                            &sampler_linear,
                            &sampler_nearest,
                            &out,
                        );
                        let _ = device.poll(wgpu::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: None,
                        });
                        let resident = Arc::new(ResidentImage {
                            image: Some(image),
                            drop_tx: Some(drop_tx.clone()),
                            tiles: None,
                        });
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

/// Pack the per-draw uniform block to match the shader `Uniforms` struct (80 bytes):
/// the dst/src rects (0..32), the flags `UVec4` (32..48, x = sRGB, y = kernel), then
/// footprint, tex_size, and the cubic `(B, C)` (48..72; 72..80 is tail padding).
#[allow(clippy::too_many_arguments)]
fn build_uniforms(
    dst: [f32; 4],
    src: [f32; 4],
    is_srgb: bool,
    kernel: u32,
    footprint: [f32; 2],
    tex_size: [f32; 2],
    bc: [f32; 2],
) -> [u8; 80] {
    let mut buf = [0u8; 80];
    let floats = [
        dst[0], dst[1], dst[2], dst[3], src[0], src[1], src[2], src[3],
    ];
    for (i, f) in floats.iter().enumerate() {
        buf[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf[32..36].copy_from_slice(&(is_srgb as u32).to_le_bytes());
    buf[36..40].copy_from_slice(&kernel.to_le_bytes());
    let tail = [
        footprint[0],
        footprint[1],
        tex_size[0],
        tex_size[1],
        bc[0],
        bc[1],
    ];
    for (i, f) in tail.iter().enumerate() {
        let o = 48 + i * 4;
        buf[o..o + 4].copy_from_slice(&f.to_le_bytes());
    }
    buf
}

const IMAGE_SPV: &[u8] = include_bytes!("image.spv");
