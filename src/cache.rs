//! Thin wrapper around iced's image allocation system.
//!
//! Images are loaded through `image::allocate()`, which returns a `Task` that
//! resolves to an `Allocation`, a GPU-resident texture guaranteed to render
//! immediately on the next frame with no flicker.
//!
//! Pre-fetching works by firing `image::allocate()` tasks for neighbor images
//! in `update()`. Holding the resulting `Allocation`s keeps iced's internal
//! texture memory alive. By the time the user navigates, the texture is ready.

use std::path::Path;

pub use iced::widget::image::{Allocation, Error};

use iced::Task;
use iced::widget::image::{self, Handle};

/// Create a Handle for the given image path.
///
/// Uses `Handle::from_path` so iced produces a deterministic, path-hash-based
/// `Id`. The same path always yields the same `Id`.
#[allow(dead_code)]
pub fn handle_for(path: &Path) -> Handle {
    Handle::from_path(path)
}

/// Kick off allocation of an image at `path`.
///
/// Returns a `Task` that resolves to an `Allocation` once iced has decoded
/// and uploaded the image to GPU memory. Holding the `Allocation` guarantees
/// the texture is ready for immediate, flicker-free display.
pub fn allocate_path(path: &Path) -> Task<Result<Allocation, Error>> {
    image::allocate(Handle::from_path(path))
}
