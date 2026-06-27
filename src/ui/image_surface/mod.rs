//! GPU still-image surface: scryglass owns the RGBA texture and renders it
//! through a shader, per window, so a freshly shown image never flickers the
//! way iced's first-window-only atlas upload does. Zoom, pan, and fit reuse the
//! shared display geometry, so stills and video share one path.

mod pipeline;
mod program;

#[cfg(test)]
pub use pipeline::test_keepalive;
pub use pipeline::{Keepalive, ResidentImage, submit_upload, submit_upload_at};
pub use program::{view, warmup};
