//! The dedicated upload thread and its job channel: texture uploads, GPU
//! view-res downscales, and off-thread texture frees, all serialized so the
//! render thread never waits on VRAM traffic.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use iced::wgpu;
use iced::widget::image::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::DownscaleKernel;

use super::resident::{GpuImage, Keepalive, ResidentImage};
use super::uniforms::{UNIFORM_SIZE, build_uniforms};

/// Hands decoded images to the single dedicated upload thread (set up on the
/// pipeline's first build, the only place wgpu exposes the device). One thread
/// keeps uploads serialized, so concurrent 64 MB writes never contend and the
/// tokio pool stays free for decoding.
pub(super) struct UploadContext {
    pub(super) jobs: UnboundedSender<Job>,
    pub(super) max_dim: u32,
    /// The kernel the last display draw used, so the off-thread view-res render
    /// downscales with exactly what is on screen. Read by the render job, written
    /// by every draw, so it tracks the live `downscale_kernel` with no plumbing
    /// through the load/prefetch call graph.
    pub(super) kernel: AtomicU8,
    /// The display scale factor the last draw reported (`f32` bits), so a view-res
    /// copy is sized to the physical display rather than a fixed headroom guess.
    pub(super) scale_factor: AtomicU32,
}

/// Work for the upload thread. Upload creates a texture off the render thread.
/// Drop frees one off the render thread too, since a 64 MB VRAM free on the
/// render thread stalls the frame (iced drops on its worker for the same reason).
pub(super) enum Job {
    Upload {
        handle: Handle,
        /// Resolved with the keepalive once the texture is resident. The app
        /// holds it for as long as it wants the image. Dropping it frees the
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
    /// Overwrite an existing animation texture in place, no allocation. An
    /// animation's dimensions never change frame to frame, so one texture serves
    /// its whole life and its bind groups are reused. The same keepalive is
    /// returned once its texture holds the new frame.
    WriteFrame {
        handle: Handle,
        into: Keepalive,
        ready: tokio::sync::oneshot::Sender<Keepalive>,
    },
    Drop(GpuImage),
}

pub(super) static UPLOAD_CONTEXT: OnceLock<UploadContext> = OnceLock::new();

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

/// Build a texture's two bind groups (linear and nearest sampling). Takes the
/// texture by value: the image owns it so the drop path can destroy it.
fn bind_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler_linear: &wgpu::Sampler,
    sampler_nearest: &wgpu::Sampler,
    texture: wgpu::Texture,
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
                        // One slot's worth. The dynamic offset picks which.
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
    let size = (texture.width(), texture.height());
    GpuImage {
        bind_linear,
        bind_nearest,
        view,
        texture,
        size,
    }
}

/// Stage `pixels` through the recycled belt and copy them into `texture`,
/// growing `staging` when the aligned upload outsizes it. Returns the submission
/// so the caller waits it out. Shared by a fresh upload and an in-place animation
/// frame write.
#[allow(clippy::too_many_arguments)]
fn stage_copy(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    belt: &mut wgpu::util::StagingBelt,
    staging: &mut Option<wgpu::Buffer>,
    staging_cap: &mut u64,
    width: u32,
    height: u32,
    pixels: &[u8],
    texture: &wgpu::Texture,
) -> wgpu::SubmissionIndex {
    let bytes_per_row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let total = bytes_per_row as u64 * height as u64;
    if *staging_cap < total {
        *staging = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scryglass image staging"),
            size: total,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        *staging_cap = total;
    }
    let staging_buf = staging.as_ref().expect("staging buffer");
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("scryglass image upload"),
    });
    // Copy each row into recycled staging at the aligned row stride, then
    // schedule the buffer-to-texture copy.
    if let Some(size) = wgpu::BufferSize::new(total) {
        let mut view = belt.write_buffer(&mut encoder, staging_buf, 0, size, device);
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
            texture,
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
    submission
}

/// Whether the upload thread exists yet. It is built by the first frame any
/// window draws, which a window revealed late (a maximized relaunch) delays.
pub fn upload_ready() -> bool {
    UPLOAD_CONTEXT.get().is_some()
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

/// Queue a new animation frame to be written into an existing texture in place,
/// no allocation. Returns false (the caller then falls back to a fresh upload)
/// when the pipeline is not built yet, the handle is not Rgba, or its dimensions
/// do not match the target texture (a re-decode changed size).
pub fn submit_write_frame(
    handle: Handle,
    into: Keepalive,
    ready: tokio::sync::oneshot::Sender<Keepalive>,
) -> bool {
    let Some(ctx) = UPLOAD_CONTEXT.get() else {
        return false;
    };
    let Handle::Rgba { width, height, .. } = &handle else {
        return false;
    };
    let Some((_, (tw, th))) = into.write_target() else {
        return false;
    };
    if *width != tw || *height != th {
        return false;
    }
    ctx.jobs
        .send(Job::WriteFrame {
            handle,
            into,
            ready,
        })
        .is_ok()
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
pub(super) struct UploadThread {
    pub(super) device: wgpu::Device,
    pub(super) queue: wgpu::Queue,
    pub(super) layout: wgpu::BindGroupLayout,
    pub(super) uniforms: wgpu::Buffer,
    pub(super) sampler_linear: wgpu::Sampler,
    pub(super) sampler_nearest: wgpu::Sampler,
    pub(super) render_pipeline: wgpu::RenderPipeline,
    pub(super) render_uniforms: wgpu::Buffer,
    pub(super) empty_bind: wgpu::BindGroup,
}

/// The dedicated upload thread, modeled on iced's image worker. It drains jobs
/// one at a time and waits out each upload on this thread, so only one is ever
/// in flight (back-pressure) and no frame stalls on the wait. View-res renders
/// and texture frees (Job::Drop) also run here.
pub(super) fn spawn_upload_thread(
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
            // like iced's worker, so GPU staging is not allocated per image.
            // `staging` is the reused copy-source buffer. It lives for one
            // burst of jobs and is freed when the queue idles, so the largest
            // upload ever made does not pin its size in VRAM for good.
            let mut belt = wgpu::util::StagingBelt::new(4 * 1024 * 1024);
            let mut staging: Option<wgpu::Buffer> = None;
            let mut staging_cap: u64 = 0;
            loop {
                let job = match jobs.try_recv() {
                    Ok(job) => job,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        // The burst is over and every submission that used the
                        // buffer was waited out, so destroy it before sleeping.
                        // The poll has the driver reclaim it now.
                        if let Some(buf) = staging.take() {
                            staging_cap = 0;
                            buf.destroy();
                            let _ = device.poll(wgpu::PollType::Poll);
                        }
                        match jobs.blocking_recv() {
                            Some(job) => job,
                            None => break,
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                };
                match job {
                    Job::Upload { handle, ready } => {
                        let Handle::Rgba {
                            width,
                            height,
                            pixels,
                            ..
                        } = &handle
                        else {
                            // Never reached (loads decode to Rgba). Drop `ready`
                            // so the awaiter sees no keepalive.
                            continue;
                        };
                        let (width, height) = (*width, *height);
                        let texture = create_rgba_texture(&device, width, height, false);
                        let submission = stage_copy(
                            &device,
                            &queue,
                            &mut belt,
                            &mut staging,
                            &mut staging_cap,
                            width,
                            height,
                            pixels,
                            &texture,
                        );
                        let image = bind_texture(
                            &device,
                            &layout,
                            &uniforms,
                            &sampler_linear,
                            &sampler_nearest,
                            texture,
                        );
                        // Wait for the GPU here so only one upload is ever in
                        // flight (iced's back-pressure), off the render thread.
                        let _ = device.poll(wgpu::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: None,
                        });
                        // The app holds this Arc (keeping the texture resident).
                        // Dropping the last Arc frees the texture via `drop_tx`.
                        let resident = ResidentImage::texture(image, drop_tx.clone());
                        let _ = ready.send(resident);
                    }
                    Job::WriteFrame {
                        handle,
                        into,
                        ready,
                    } => {
                        let Handle::Rgba {
                            width,
                            height,
                            pixels,
                            ..
                        } = &handle
                        else {
                            // Never reached (frames composite to Rgba). Drop
                            // `ready` so the awaiter sees no keepalive.
                            continue;
                        };
                        let Some((texture, (tw, th))) = into.write_target() else {
                            // Not a single texture (drained or tiled). Drop
                            // `ready` so the caller falls back to a fresh upload.
                            continue;
                        };
                        let (width, height) = (*width, *height);
                        // submit_write_frame already gated on this. Guard again so
                        // a texture is never written with mismatched dimensions.
                        if width != tw || height != th {
                            continue;
                        }
                        let submission = stage_copy(
                            &device,
                            &queue,
                            &mut belt,
                            &mut staging,
                            &mut staging_cap,
                            width,
                            height,
                            pixels,
                            texture,
                        );
                        // Wait like Job::Upload: the copy fully completes before
                        // any later-submitted draw samples the texture, so writing
                        // the live displayed frame never tears.
                        let _ = device.poll(wgpu::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: None,
                        });
                        // Return the SAME keepalive, its texture now holding the
                        // new frame. No texture or bind groups were created.
                        let _ = ready.send(into);
                    }
                    Job::RenderDownscale {
                        source,
                        target,
                        ready,
                    } => {
                        // Skip the tokenless test keepalive. The caller then falls
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
                            out,
                        );
                        let _ = device.poll(wgpu::PollType::Wait {
                            submission_index: Some(submission),
                            timeout: None,
                        });
                        let resident = ResidentImage::texture(image, drop_tx.clone());
                        let _ = ready.send(resident);
                    }
                    Job::Drop(image) => {
                        // Dropping the handles alone does not free the native
                        // texture for a minimized window (wgpu's last internal
                        // reference never unwinds). destroy() plus the poll has
                        // the driver reclaim the VRAM now. The app never draws
                        // a dropped image again, so nothing can submit it.
                        image.texture.destroy();
                        drop(image);
                        let _ = device.poll(wgpu::PollType::Poll);
                    }
                }
            }
        })
        .expect("spawn scryglass image upload thread");
}

pub(super) fn sampler_desc(filter: wgpu::FilterMode) -> wgpu::SamplerDescriptor<'static> {
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
