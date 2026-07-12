//! GPU still-image surface: scryglass owns the RGBA texture and renders it
//! through a shader, per window, so a freshly shown image never flickers the
//! way iced's first-window-only atlas upload does. Zoom, pan, and fit reuse the
//! shared display geometry, so stills and video share one path.

mod pipeline;
mod program;
mod resident;
mod tiles;
mod uniforms;
mod upload;

pub use program::{view, warmup};
#[cfg(test)]
pub use resident::test_keepalive;
pub use resident::{Keepalive, ResidentImage};
pub use tiles::{DrawWant, MAX_TILE_DRAWS, TileSet};
pub use upload::{
    current_kernel, current_scale_factor, max_texture_dim, submit_render_downscale, submit_upload,
    submit_write_frame, upload_ready,
};
