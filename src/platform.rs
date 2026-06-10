//! Platform-specific operations: clipboard, shell integration.
//!
//! Provides cross-platform abstractions that dispatch to native APIs
//! on Windows and fall back to simpler approaches on other platforms.

use std::path::Path;

// ---------------------------------------------------------------------------
// Copy image to clipboard
// ---------------------------------------------------------------------------

/// Copy an image file to the system clipboard.
///
/// Uses `arboard` which supports Windows, macOS, and Linux (X11/Wayland).
/// The file is placed on the clipboard as a file list entry, so it can be
/// pasted into applications that accept file drops (image editors, chat
/// apps, file managers, etc.).
pub fn copy_image_to_clipboard(path: &Path) {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return;
    };
    let _ = clipboard.set().file_list(&[path]);
}

// ---------------------------------------------------------------------------
// Open image location (reveal in file manager)
// ---------------------------------------------------------------------------

/// Open the file's parent folder in the native file manager and highlight
/// (select) the file.
///
/// On Windows this uses `SHOpenFolderAndSelectItems`, which reuses an
/// existing Explorer window if the folder is already open, and highlights
/// the target file.
///
/// On other platforms this uses the `open` crate to open the parent folder.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    windows::reveal_in_file_manager(path);

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(parent) = path.parent() {
            let _ = open::that(parent);
        }
    }
}

// ---------------------------------------------------------------------------
// Image properties dialog
// ---------------------------------------------------------------------------

/// Open the native file properties dialog for the given path.
///
/// On Windows this uses `ShellExecuteW` with the `"properties"` verb,
/// which opens the standard Properties dialog.
///
/// On other platforms this is a no-op.
pub fn show_properties(path: &Path) {
    #[cfg(target_os = "windows")]
    windows::show_properties(path);

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use std::path::Path;

    /// Reveal a file in Explorer, reusing an existing window if possible.
    /// Uses `SHOpenFolderAndSelectItems` which highlights the file.
    pub fn reveal_in_file_manager(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::Com;
        use windows_sys::Win32::UI::Shell;

        unsafe {
            // SHOpenFolderAndSelectItems requires COM to be initialized.
            let _ = Com::CoInitializeEx(std::ptr::null(), Com::COINIT_APARTMENTTHREADED as u32);

            // Parse the path into an ITEMIDLIST (PIDL).
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let mut pidl: *mut Shell::Common::ITEMIDLIST = std::ptr::null_mut();
            let hr = Shell::SHParseDisplayName(
                wide.as_ptr(),
                std::ptr::null_mut(),
                &mut pidl,
                0,
                std::ptr::null_mut(),
            );

            if hr == 0 && !pidl.is_null() {
                // Open (or reuse) the folder window and select the item.
                Shell::SHOpenFolderAndSelectItems(pidl, 0, std::ptr::null(), 0);
                Com::CoTaskMemFree(pidl as *const _);
            }
        }
    }

    /// Open the Windows Properties dialog for a file.
    /// Uses `ShellExecuteExW` with the `"properties"` verb and
    /// `SEE_MASK_INVOKEIDLIST` flag, which is required for shell verbs
    /// like "properties" to work.
    pub fn show_properties(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell;

        let verb: Vec<u16> = "properties\0".encode_utf16().collect();
        let file: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SEE_MASK_INVOKEIDLIST = 0x0000000C, required for verbs like "properties".
        const SEE_MASK_INVOKEIDLIST: u32 = 0x0000_000C;

        let mut info = Shell::SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<Shell::SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            hwnd: std::ptr::null_mut(),
            lpVerb: verb.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: std::ptr::null(),
            lpDirectory: std::ptr::null(),
            nShow: 0, // SW_HIDE, the dialog manages its own window
            hInstApp: std::ptr::null_mut(),
            lpIDList: std::ptr::null_mut(),
            lpClass: std::ptr::null(),
            hkeyClass: std::ptr::null_mut(),
            dwHotKey: 0,
            Anonymous: unsafe { std::mem::zeroed() },
            hProcess: std::ptr::null_mut(),
        };

        unsafe {
            Shell::ShellExecuteExW(&mut info);
        }
    }
}
