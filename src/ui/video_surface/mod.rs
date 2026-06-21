//! GPU video surface: uploads decoded YUV planes and converts them to RGB
//! in a shader. Playback never pays for a CPU color conversion or a
//! per-frame upload of full RGBA, and the planes are 1.5 bytes per pixel
//! instead of 4. Zoom, pan, and fit reuse the still-image display math, so
//! video and stills share one geometry and never diverge.

mod geometry;
mod pipeline;
mod program;

pub use program::view;
