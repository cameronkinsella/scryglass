//! Thin wrapper around iced's GPU image allocation.
//!
//! `image::allocate()` returns a `Task` that resolves to an `Allocation`,
//! a GPU-resident texture guaranteed to render immediately on the next
//! frame with no flicker. Decoding happens in [`crate::media`], this module
//! only handles the upload step.

pub use iced::widget::image::{Allocation, Error};

use iced::Task;
use iced::widget::image::{self, Handle};

/// Kick off allocation of a pre-built `Handle` (decoded RGBA pixels).
pub fn allocate_handle(handle: Handle) -> Task<Result<Allocation, Error>> {
    image::allocate(handle)
}
