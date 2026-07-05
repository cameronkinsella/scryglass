//! GPU-resident still images and the app-held keepalive that owns them.
//! Textures are freed off the render thread through the upload channel, so a
//! minimized or closed window reclaims its VRAM at once.

use std::sync::Arc;

use iced::wgpu;
use tokio::sync::mpsc::UnboundedSender;

use super::tiles::TileSet;
use super::upload::Job;

/// A GPU-resident image, owned by the app through its [`Keepalive`]. Dropping
/// the last reference frees the texture off the render thread at once, so a
/// minimized or closed window reclaims its VRAM rather than waiting for some
/// later frame to sweep it.
///
/// A still too large for one texture is resident as a [`TileSet`] instead of a
/// single texture. Its tiles are small resident images themselves, so upload
/// and off-thread VRAM release work the same way tile by tile.
pub struct ResidentImage {
    body: Resident,
}

/// The forms a resident image takes. Each keepalive is exactly one of these.
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

    /// The texture to overwrite in place for an animation's next frame, with its
    /// size. `None` for a tile pyramid or the tokenless test keepalive.
    pub(super) fn write_target(&self) -> Option<(&wgpu::Texture, (u32, u32))> {
        match &self.body {
            Resident::Texture { image, .. } => Some((&image.texture, image.size)),
            Resident::Tiled(_) | Resident::Empty => None,
        }
    }

    /// The texture view to sample when rendering this image into another texture
    /// (the view-res downscale), or `None` for the tokenless test keepalive.
    pub(super) fn input_view(&self) -> Option<&wgpu::TextureView> {
        match &self.body {
            Resident::Texture { image, .. } => Some(&image.view),
            Resident::Tiled(_) | Resident::Empty => None,
        }
    }

    /// The single texture's bind group, if this resident is one.
    pub(super) fn bind(&self, nearest: bool) -> Option<&wgpu::BindGroup> {
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
            body: Resident::Tiled(TileSet::new(original, base)),
        })
    }

    /// Wrap an uploaded texture in the keepalive the app holds. `drop_tx`
    /// frees it off the render thread when the last clone drops.
    pub(super) fn texture(image: GpuImage, drop_tx: UnboundedSender<Job>) -> Keepalive {
        Arc::new(ResidentImage {
            body: Resident::Texture { image, drop_tx },
        })
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
/// (a refcount bump). The texture lives until the last clone drops.
pub type Keepalive = Arc<ResidentImage>;

/// A keepalive with no texture, for tests that only need its refcount token.
#[cfg(test)]
pub fn test_keepalive() -> Keepalive {
    Arc::new(ResidentImage {
        body: Resident::Empty,
    })
}

pub(super) struct GpuImage {
    pub(super) bind_linear: wgpu::BindGroup,
    pub(super) bind_nearest: wgpu::BindGroup,
    /// Kept so this texture can be sampled as the source of a view-res render.
    pub(super) view: wgpu::TextureView,
    /// Owned so the drop path can `destroy()` the native texture: dropping the
    /// handles only releases them for whenever wgpu's internal references
    /// unwind, which never happens while every window is minimized.
    pub(super) texture: wgpu::Texture,
    pub(super) size: (u32, u32),
}
