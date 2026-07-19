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
/// working set. The pages fault back in when next touched. Windows only, like
/// its callers: elsewhere the kernel reclaims idle memory on its own.
#[cfg(target_os = "windows")]
pub fn trim_working_set() {
    windows::trim_working_set();
}

/// `restored_pos` carried onto the monitor holding `window_center`, or None
/// when it already lies there or monitors cannot be queried.
pub fn restored_pos_on_current_monitor(
    window_center: (f32, f32),
    restored_pos: (f32, f32),
    restored_size: (f32, f32),
) -> Option<(f32, f32)> {
    #[cfg(target_os = "windows")]
    {
        windows::restored_pos_on_current_monitor(window_center, restored_pos, restored_size)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (window_center, restored_pos, restored_size);
        None
    }
}

/// Shift `pos` to the same offset in another work area (left, top, right,
/// bottom), clamped so a `size` rect stays inside it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn shift_between_work_areas(
    old: (f32, f32, f32, f32),
    new: (f32, f32, f32, f32),
    pos: (f32, f32),
    size: (f32, f32),
) -> (f32, f32) {
    let x = pos.0 - old.0 + new.0;
    let y = pos.1 - old.1 + new.1;
    (
        x.min(new.2 - size.0).max(new.0),
        y.min(new.3 - size.1).max(new.1),
    )
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

    /// The nearest monitor's handle and work area for `point`.
    fn work_area_at(point: (f32, f32)) -> Option<(isize, (f32, f32, f32, f32))> {
        use windows_sys::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };

        let pt = Foundation::POINT {
            x: point.0 as i32,
            y: point.1 as i32,
        };
        // SAFETY: MONITORINFO is plain C data valid all-zero, cbSize is set as
        // the API requires, and GetMonitorInfoW only writes into it. A failed
        // call answers "unknown".
        unsafe {
            let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            let w = info.rcWork;
            Some((
                monitor as isize,
                (w.left as f32, w.top as f32, w.right as f32, w.bottom as f32),
            ))
        }
    }

    pub fn restored_pos_on_current_monitor(
        window_center: (f32, f32),
        restored_pos: (f32, f32),
        restored_size: (f32, f32),
    ) -> Option<(f32, f32)> {
        let (current, current_work) = work_area_at(window_center)?;
        let restored_center = (
            restored_pos.0 + restored_size.0 / 2.0,
            restored_pos.1 + restored_size.1 / 2.0,
        );
        let (restored, restored_work) = work_area_at(restored_center)?;
        if current == restored {
            return None;
        }
        Some(super::shift_between_work_areas(
            restored_work,
            current_work,
            restored_pos,
            restored_size,
        ))
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

#[cfg(test)]
mod tests {
    use super::shift_between_work_areas;

    #[test]
    fn shifting_between_work_areas_keeps_the_offset() {
        let old = (0.0, 0.0, 2560.0, 1392.0);
        let new = (2560.0, 0.0, 5120.0, 1392.0);
        let pos = shift_between_work_areas(old, new, (300.0, 200.0), (900.0, 650.0));
        assert_eq!(pos, (2860.0, 200.0));
    }

    #[test]
    fn a_shifted_window_is_clamped_into_the_new_area() {
        // A smaller monitor: the same offset would hang off the right edge.
        let old = (0.0, 0.0, 2560.0, 1392.0);
        let new = (2560.0, 0.0, 3840.0, 976.0);
        let pos = shift_between_work_areas(old, new, (2000.0, 800.0), (900.0, 650.0));
        assert_eq!(pos, (2940.0, 326.0));

        // A window larger than the area rests at its origin.
        let pos = shift_between_work_areas(old, new, (100.0, 100.0), (3000.0, 2000.0));
        assert_eq!(pos, (2560.0, 0.0));
    }
}
