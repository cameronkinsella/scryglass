//! Thin wrapper around iced's GPU image allocation. `image::allocate()`
//! resolves to an `Allocation`, a GPU texture that renders flicker-free on
//! the next frame. Decoding lives in [`crate::media`]. This handles only
//! the upload.
//!
//! Imports come from `iced_runtime` directly because the facade re-exports
//! them only under its full `image` feature, which would pull in the codec
//! crates the pipeline replaces.

pub use iced_runtime::image::{Allocation, Error};

use iced::Task;
use iced::widget::image::Handle;

/// Kick off allocation of a pre-built `Handle` (decoded RGBA pixels).
pub fn allocate_handle(handle: Handle) -> Task<Result<Allocation, Error>> {
    iced_runtime::image::allocate(handle)
}
