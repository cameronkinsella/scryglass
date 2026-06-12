//! Thin wrapper around iced's GPU image allocation.
//!
//! `image::allocate()` returns a `Task` that resolves to an `Allocation`,
//! a GPU-resident texture guaranteed to render immediately on the next
//! frame with no flicker. Decoding happens in [`crate::media`], this module
//! only handles the upload step.
//!
//! The imports come from `iced_runtime` directly: the facade re-exports
//! them only under its full `image` feature, which would drag in codec
//! crates the pipeline replaces (`image-without-codecs` is on instead).

pub use iced_runtime::image::{Allocation, Error};

use iced::Task;
use iced::widget::image::Handle;

/// Kick off allocation of a pre-built `Handle` (decoded RGBA pixels).
pub fn allocate_handle(handle: Handle) -> Task<Result<Allocation, Error>> {
    iced_runtime::image::allocate(handle)
}
