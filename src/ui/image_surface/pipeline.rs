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
use crate::ui::image_display::snap_footprint_to_unit;

const UNIFORM_SIZE: u64 = 80;

/// Uniform slots one frame can draw: slot 0 for the single texture (or a tile
/// pyramid's base layer), the rest for visible tiles. The LOD floor leaves up
/// to 2 level texels per physical pixel, so a 4K viewport can show
/// ceil(3840*2/512)+1 x ceil(2160*2/512)+1 = 16x10 tiles, 17x11 with the
/// demand margin. 192 covers that with headroom; a larger display degrades to
/// the base layer past the cap.
const UNIFORM_SLOTS: u64 = 192;

/// Byte stride between slots: the WebGPU default limit for
/// `minUniformBufferOffsetAlignment`, https://www.w3.org/TR/webgpu/#limits
const UNIFORM_STRIDE: u64 = 256;

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
    body: Resident,
}

/// The forms a resident image takes; each keepalive is exactly one of these.
enum Resident {
    /// One texture, freed off the render thread through the channel.
    Texture {
        image: GpuImage,
        drop_tx: UnboundedSender<Job>,
    },
    /// A tile pyramid for a still too large for one texture.
    Tiled(TileSet),
    /// No GPU state: the tokenless test keepalive, or a drained drop.
    Empty,
}

impl ResidentImage {
    /// The resident texture's size in texels, or `None` for a tile pyramid or
    /// the tokenless test keepalive. The downscale shader needs it to step
    /// taps by whole texels.
    pub fn size(&self) -> Option<(u32, u32)> {
        match &self.body {
            Resident::Texture { image, .. } => Some(image.size),
            Resident::Tiled(_) | Resident::Empty => None,
        }
    }

    /// The texture view to sample when rendering this image into another texture
    /// (the view-res downscale), or `None` for the tokenless test keepalive.
    fn input_view(&self) -> Option<&wgpu::TextureView> {
        match &self.body {
            Resident::Texture { image, .. } => Some(&image.view),
            Resident::Tiled(_) | Resident::Empty => None,
        }
    }

    /// The single texture's bind group, if this resident is one.
    fn bind(&self, nearest: bool) -> Option<&wgpu::BindGroup> {
        match &self.body {
            Resident::Texture { image, .. } => Some(if nearest {
                &image.bind_nearest
            } else {
                &image.bind_linear
            }),
            Resident::Tiled(_) | Resident::Empty => None,
        }
    }

    /// The tile pyramid, when this resident is a tiled still.
    pub fn tiles(&self) -> Option<&TileSet> {
        match &self.body {
            Resident::Tiled(set) => Some(set),
            Resident::Texture { .. } | Resident::Empty => None,
        }
    }

    /// A fresh, empty tile pyramid for an `original`-sized still. Purely
    /// CPU-side bookkeeping: the VRAM arrives tile by tile as each one is
    /// produced and uploaded like any small image. `base` is the view-quality
    /// texture drawn stretched beneath the tiles, so a not-yet-produced tile
    /// shows a softer image instead of a hole.
    pub fn tiled(original: (u32, u32), base: Keepalive) -> Keepalive {
        Arc::new(ResidentImage {
            body: Resident::Tiled(TileSet {
                original,
                base: Mutex::new(base),
                exact: Mutex::new(ExactLayer {
                    target: (0, 0),
                    tiles: TileCache::new(MAX_EXACT_TILES),
                    pending: std::collections::HashMap::new(),
                }),
                tiles: Mutex::new(TileCache::new(MAX_CACHED_TILES)),
                pending: Mutex::new(std::collections::HashMap::new()),
                wanted_lod: AtomicU32::new(0),
                draw_lod: AtomicU32::new(DRAW_UNSTAMPED),
                draw_scale: AtomicU32::new(1.0f32.to_bits()),
                draw_shown: std::sync::atomic::AtomicU64::new(0),
            }),
        })
    }
}

/// Most tiles a pyramid keeps resident: a 4K viewport's worst-case wanted set
/// (see [`UNIFORM_SLOTS`]) plus headroom, so a demand wave never evicts tiles
/// it just produced. Older tiles drop (freeing their VRAM) and are re-produced
/// from the RAM source on return.
const MAX_CACHED_TILES: usize = 224;

/// Most exact-scale tiles kept resident: the visible set plus pan margin
/// (a 4K viewport at one texel per pixel shows ceil(3840/512+1) x
/// ceil(2160/512+1) = 9x6 = 54). All drop together when the resting size
/// changes.
const MAX_EXACT_TILES: usize = 96;

/// The exact-scale layer: tiles that are byte-exact crops of the one-pass
/// downscale at `target`, keyed by grid position with `lod` fixed at 0.
struct ExactLayer {
    /// The whole-image size the tiles are exact for; (0, 0) before the
    /// first rest.
    target: (u32, u32),
    tiles: TileCache<Keepalive>,
    pending: std::collections::HashMap<TileKey, std::time::Instant>,
}

/// The resident form of a tiled still: its bounded tile cache plus the
/// uncapped source size the tile grid maps. Shared cross-window inside one
/// [`Keepalive`], with tiles streaming in through the mutex as they land.
pub struct TileSet {
    original: (u32, u32),
    /// The view-quality layer beneath the tiles, re-derived at resting zooms
    /// so it stays a one-pass copy (a fixed base redrawn through the kernel
    /// would be softer than the View tier it must match).
    base: Mutex<Keepalive>,
    /// Exact-scale tiles for the resting display size: byte-exact crops of
    /// the one-pass whole-image downscale, produced for the visible region
    /// only, so a rest costs viewport work instead of whole-image work.
    /// Replaced whole when the resting size changes.
    exact: Mutex<ExactLayer>,
    /// The displayed size the last tiled draw spanned, packed `w << 32 | h`.
    /// The demand pass targets exactly this, so the draw's size test and
    /// the produced tiles can never disagree by a rounding step.
    draw_shown: std::sync::atomic::AtomicU64,
    tiles: Mutex<TileCache<Keepalive>>,
    /// Tiles requested but not yet landed, with their claim time, so a pan or
    /// zoom repeating its demand pass never produces the same tile twice. A
    /// claim whose settle message was lost (its window closed mid-production)
    /// expires rather than blocking the tile for the pyramid's lifetime.
    pending: Mutex<std::collections::HashMap<TileKey, std::time::Instant>>,
    /// The mip level the latest demand pass asked for. A queued production
    /// for another level bails before its resample: a zoom that keeps moving
    /// obsoletes whole waves of tiles, and this is what stops them from
    /// being produced anyway.
    wanted_lod: AtomicU32,
    /// What the last tiled draw actually selected, stamped by `prepare_tiles`
    /// and read by the demand pass, so production always targets the level
    /// the real placement samples (one source of truth for scale and
    /// rounding). Holds [`DRAW_UNSTAMPED`] before any tiled draw and
    /// [`DRAW_BASE_ONLY`] when the base layer alone sufficed.
    draw_lod: AtomicU32,
    /// The scale factor of the last tiled draw (`f32` bits), so the demand
    /// pass works in the physical pixels of the window actually drawing.
    draw_scale: AtomicU32,
}

/// Most tiles one demand pass may claim: what one frame can draw.
pub const MAX_TILE_DRAWS: usize = UNIFORM_SLOTS as usize - 1;

/// `draw_lod` sentinel: no tiled draw has resolved a level yet.
const DRAW_UNSTAMPED: u32 = u32::MAX;
/// `draw_lod` sentinel: the last draw needed no tiles at all.
const DRAW_BASE_ONLY: u32 = u32::MAX - 1;

/// How long a tile claim blocks re-requests before it is presumed lost. Far
/// past any real produce plus upload, so it only fires for a settle message
/// that will never arrive.
const CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// What the demand pass should do, read back from the last real draw.
pub enum DrawWant {
    /// No tiled draw has happened; the caller derives the level itself.
    Unknown,
    /// The base layer sufficed; no tiles are needed.
    BaseOnly,
    /// The draw sampled this level.
    Level(u32),
}

impl TileSet {
    /// The uncapped source dimensions the pyramid maps.
    pub fn original(&self) -> (u32, u32) {
        self.original
    }

    /// The view-quality layer beneath the tiles.
    pub fn base(&self) -> Option<Keepalive> {
        self.base.lock().ok().map(|base| base.clone())
    }

    /// Ready the exact layer for `target`, dropping tiles of any other size
    /// (their VRAM frees off-thread as the keepalives drop).
    pub fn ensure_exact(&self, target: (u32, u32)) {
        if let Ok(mut layer) = self.exact.lock()
            && layer.target != target
        {
            layer.target = target;
            layer.tiles = TileCache::new(MAX_EXACT_TILES);
            layer.pending.clear();
        }
    }

    /// The size the exact layer currently serves.
    pub fn exact_target(&self) -> (u32, u32) {
        self.exact
            .lock()
            .map(|layer| layer.target)
            .unwrap_or((0, 0))
    }

    /// Claim one exact tile for production: false when it is resident, in
    /// flight (unexpired), or the layer moved to another size.
    pub fn try_claim_exact(&self, target: (u32, u32), key: TileKey) -> bool {
        self.exact
            .lock()
            .map(|mut layer| {
                if layer.target != target || layer.tiles.contains(key) {
                    return false;
                }
                match layer.pending.get(&key) {
                    Some(claimed) if claimed.elapsed() < CLAIM_TTL => false,
                    _ => {
                        layer.pending.insert(key, std::time::Instant::now());
                        true
                    }
                }
            })
            .unwrap_or(false)
    }

    /// A production for `target` finished: release its claim and install the
    /// texture, unless the layer moved to another size in the meantime.
    pub fn settle_exact(&self, target: (u32, u32), key: TileKey, texture: Option<Keepalive>) {
        if let Ok(mut layer) = self.exact.lock()
            && layer.target == target
        {
            layer.pending.remove(&key);
            if let Some(texture) = texture {
                layer.tiles.insert(key, texture);
            }
        }
    }

    /// A resident exact tile for `target`, refreshing its recency.
    fn exact_get(&self, target: (u32, u32), key: TileKey) -> Option<Keepalive> {
        self.exact.lock().ok().and_then(|mut layer| {
            if layer.target != target {
                return None;
            }
            layer.tiles.get(key).cloned()
        })
    }

    /// The displayed size the last tiled draw spanned.
    pub fn draw_shown(&self) -> (u32, u32) {
        let packed = self.draw_shown.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }

    /// Production started: restart the tile's claim clock, so the TTL
    /// measures the work, not the queue behind the gate.
    pub fn refresh_claim(&self, key: TileKey) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(claimed) = pending.get_mut(&key)
        {
            *claimed = std::time::Instant::now();
        }
    }

    /// The scale factor of the last tiled draw.
    pub fn draw_scale(&self) -> f32 {
        f32::from_bits(self.draw_scale.load(Ordering::Relaxed))
    }

    /// Install a produced tile, evicting the stalest past the cap.
    pub fn insert(&self, key: TileKey, tile: Keepalive) {
        if let Ok(mut tiles) = self.tiles.lock() {
            tiles.insert(key, tile);
        }
    }

    /// The tile for `key`, freshly marked as used.
    pub fn get(&self, key: TileKey) -> Option<Keepalive> {
        self.tiles.lock().ok()?.get(key).cloned()
    }

    /// Claim `key` for production: true when it is neither resident nor
    /// already in flight, marking it in flight. An expired claim (its settle
    /// message was lost) counts as absent.
    pub fn try_claim(&self, key: TileKey) -> bool {
        let resident = self
            .tiles
            .lock()
            .map(|tiles| tiles.contains(key))
            .unwrap_or(true);
        if resident {
            return false;
        }
        self.pending
            .lock()
            .map(|mut pending| match pending.get(&key) {
                Some(claimed) if claimed.elapsed() < CLAIM_TTL => false,
                _ => {
                    pending.insert(key, std::time::Instant::now());
                    true
                }
            })
            .unwrap_or(false)
    }

    /// A production for `key` finished (either way); its claim is released.
    pub fn settle(&self, key: TileKey) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&key);
        }
    }

    /// Record the level the current view wants; stale productions bail.
    pub fn set_wanted_lod(&self, lod: u32) {
        self.wanted_lod.store(lod, Ordering::Relaxed);
    }

    /// The level the latest demand pass asked for.
    pub fn wanted_lod(&self) -> u32 {
        self.wanted_lod.load(Ordering::Relaxed)
    }

    /// What the last real draw wanted, so demand and draw cannot disagree on
    /// scale or rounding.
    pub fn draw_want(&self) -> DrawWant {
        match self.draw_lod.load(Ordering::Relaxed) {
            DRAW_UNSTAMPED => DrawWant::Unknown,
            DRAW_BASE_ONLY => DrawWant::BaseOnly,
            lod => DrawWant::Level(lod),
        }
    }
}

impl std::fmt::Debug for ResidentImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let body = match &self.body {
            Resident::Texture { .. } => "texture",
            Resident::Tiled(_) => "tiled",
            Resident::Empty => "empty",
        };
        f.debug_struct("ResidentImage")
            .field("body", &body)
            .finish()
    }
}

impl Drop for ResidentImage {
    fn drop(&mut self) {
        // Free on the upload thread, never the render thread.
        if let Resident::Texture { image, drop_tx } =
            std::mem::replace(&mut self.body, Resident::Empty)
        {
            let _ = drop_tx.send(Job::Drop(image));
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
        body: Resident::Empty,
    })
}

/// Persistent GPU state shared by every still-image draw.
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    /// Bound at slot 0; rust-gpu reserves set 0, so the real bindings are set 1.
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
        set.draw_scale.store(scale.to_bits(), Ordering::Relaxed);
        // The physical size the WHOLE image is displayed at; demand targets
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
        set.draw_shown.store(
            (u64::from(shown.0) << 32) | u64::from(shown.1),
            Ordering::Relaxed,
        );
        // Texels per physical pixel of the substrate; its inverse is the
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
            let (bdst, bsrc) = crate::ui::image_display::snap_placement_to_pixels(
                dst,
                src,
                (bw as f32, bh as f32),
                viewport_phys,
                crate::ui::image_display::near_one_to_one(base_fp),
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
            // single taps, indistinguishable from a whole exact base. The
            // placement is re-snapped against the exact grid itself, so a
            // panned view still lands taps on texel centers, and each tile
            // maps through the visible `src` window (dst only spans the
            // viewport when zoomed in).
            if shown != (0, 0) && set.exact_target() == shown {
                let (edst, esrc) = crate::ui::image_display::snap_placement_to_pixels(
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
                set.draw_lod.store(DRAW_BASE_ONLY, Ordering::Relaxed);
                return;
            }
            // The base covers the view when it is at least as fine as the
            // display OR within the near-1:1 band: float dust leaves an
            // exact base a hair either side of 1.0, and the snap only caps
            // from above, so a bare >= 1.0 test lets one axis reading
            // 0.9999 invite cascade tiles over an exact copy.
            if crate::ui::image_display::near_one_to_one(base_fp)
                || (base_fp[0] >= 1.0 && base_fp[1] >= 1.0)
            {
                set.draw_lod.store(DRAW_BASE_ONLY, Ordering::Relaxed);
                return;
            }
        }
        let lod = tiles::lod_for_zoom(1.0 / footprint[0].max(footprint[1]));
        set.draw_lod.store(lod, Ordering::Relaxed);
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

/// Where one tile lands on screen and what part of its padded texture shows:
/// the tile's payload rectangle mapped from level space through the image's
/// placement, and the source rect inset past the gutter.
fn tile_placement(
    dst: [f32; 4],
    src: [f32; 4],
    level: (u32, u32),
    key: crate::media::tiles::TileKey,
    tex: (u32, u32),
) -> ([f32; 4], [f32; 4]) {
    use crate::media::tiles::GUTTER;
    let (x, y, w, h) = crate::media::tiles::tile_rect(level, key.col, key.row);
    // The payload's rect in image UV, then through the placement's linear
    // src -> dst map. Adjacent tiles share exact edge coordinates, so the
    // rasterizer leaves no cracks.
    let (lw, lh) = (level.0 as f32, level.1 as f32);
    let map = |v: f32, s0: f32, s1: f32, d0: f32, d1: f32| d0 + (v - s0) / (s1 - s0) * (d1 - d0);
    let tdst = [
        map(x as f32 / lw, src[0], src[2], dst[0], dst[2]),
        map(y as f32 / lh, src[1], src[3], dst[1], dst[3]),
        map((x + w) as f32 / lw, src[0], src[2], dst[0], dst[2]),
        map((y + h) as f32 / lh, src[1], src[3], dst[1], dst[3]),
    ];
    let (tw, th) = (tex.0 as f32, tex.1 as f32);
    let g = GUTTER as f32;
    let tsrc = [g / tw, g / th, (g + w as f32) / tw, (g + h as f32) / th];
    (tdst, tsrc)
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
                        // One slot's worth; the dynamic offset picks which.
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: uniforms,
                            offset: 0,
                            size: wgpu::BufferSize::new(UNIFORM_SIZE),
                        }),
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
                            body: Resident::Texture {
                                image,
                                drop_tx: drop_tx.clone(),
                            },
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
                                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                        buffer: &render_uniforms,
                                        offset: 0,
                                        size: wgpu::BufferSize::new(UNIFORM_SIZE),
                                    }),
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
                            pass.set_bind_group(1, &in_bind, &[0]);
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
                            body: Resident::Texture {
                                image,
                                drop_tx: drop_tx.clone(),
                            },
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
