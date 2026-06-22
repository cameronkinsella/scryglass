//! Workaround for an idle-GPU power-management quirk, not a real fix.
//!
//! With nothing else loading the GPU, the driver parks video memory at its
//! lowest clock a few seconds into hardware-decoded playback, starving the
//! decoder until the picture stutters. A trickle of memory traffic holds the
//! clock up. It engages only after decode is seen falling behind, and latches
//! for the process.

use std::sync::atomic::{AtomicBool, Ordering};

static DECODE_BEHIND: AtomicBool = AtomicBool::new(false);

/// Flag that decode fell behind, latched for the process lifetime.
pub fn flag_decode_behind() {
    DECODE_BEHIND.store(true, Ordering::Relaxed);
}

/// Whether the renderer should run the keep-alive.
pub fn needed() -> bool {
    DECODE_BEHIND.load(Ordering::Relaxed)
}
