//! Platform-specific operations: clipboard and OS integration.

use std::path::Path;

mod present_probe;
mod shell;

pub use shell::{
    file_associations_registered, register_file_associations, reveal_in_file_manager,
    show_properties, unregister_file_associations,
};

#[cfg(target_os = "windows")]
pub use present_probe::supported_present_modes;

// ---------------------------------------------------------------------------
// Copy image to clipboard
// ---------------------------------------------------------------------------

/// Copy a file to the system clipboard as a file-list entry, so it can
/// be pasted into file managers and applications that accept file drops.
pub fn copy_file_to_clipboard(path: &Path) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return;
    };
    let _ = clipboard.set().file_list(&[path]);
}

// ---------------------------------------------------------------------------
// Working-set trim
// ---------------------------------------------------------------------------

/// Ask the OS to return this process's resident pages, shrinking its reported
/// working set. The pages fault back in when next touched. A no-op off Windows,
/// where the kernel reclaims idle memory under pressure on its own.
pub fn trim_working_set() {
    #[cfg(target_os = "windows")]
    windows::trim_working_set();
}

/// Whether the window with winit raw id `raw_id` sits in a snap layout.
/// Always false where snap layouts don't exist.
pub fn window_is_snapped(raw_id: u64) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::window_is_snapped(raw_id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = raw_id;
        false
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use windows_sys::Win32::Foundation;
    use windows_sys::Win32::UI::WindowsAndMessaging;

    /// Whether the window is in a snap layout. Windows keeps the pre-snap
    /// bounds in the placement's normal position, so a size mismatch means
    /// snapped. Sizes only: the normal position is in workspace coordinates,
    /// which shift when the taskbar sits left or top.
    pub fn window_is_snapped(raw_id: u64) -> bool {
        use WindowsAndMessaging::{GetWindowPlacement, GetWindowRect, WINDOWPLACEMENT};

        let hwnd = raw_id as *mut core::ffi::c_void;
        // SAFETY: both structs are plain C data whose all-zero pattern is
        // valid, the calls only write into them, and the handle belongs to a
        // window of this process. A failed call answers "not snapped".
        unsafe {
            let mut rect: Foundation::RECT = std::mem::zeroed();
            let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
            placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
            if GetWindowRect(hwnd, &mut rect) == 0 || GetWindowPlacement(hwnd, &mut placement) == 0
            {
                return false;
            }
            let normal = placement.rcNormalPosition;
            (rect.right - rect.left, rect.bottom - rect.top)
                != (normal.right - normal.left, normal.bottom - normal.top)
        }
    }

    /// Empty this process's working set with `EmptyWorkingSet`, moving its
    /// resident pages to the standby list so the OS can reclaim them.
    pub fn trim_working_set() {
        use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no close,
        // and EmptyWorkingSet only reads it. The call is best-effort, so a
        // failure (the BOOL result) is ignored.
        let _ = unsafe { EmptyWorkingSet(GetCurrentProcess()) };
    }
}

/// Run `work` on the current thread at below-normal scheduling priority,
/// restored after (panic-safe), so background decodes and resamples always
/// yield the CPU to the UI and foreground work.
/// https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setthreadpriority
pub(crate) fn run_below_normal<T>(work: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            set_thread_priority(false);
        }
    }
    set_thread_priority(true);
    let _restore = Restore;
    work()
}

fn set_thread_priority(lowered: bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
            THREAD_PRIORITY_NORMAL,
        };
        let priority = if lowered {
            THREAD_PRIORITY_BELOW_NORMAL
        } else {
            THREAD_PRIORITY_NORMAL
        };
        // Only the calling thread is affected. On failure it stays normal.
        unsafe { SetThreadPriority(GetCurrentThread(), priority) };
    }
    #[cfg(not(windows))]
    let _ = lowered;
}
