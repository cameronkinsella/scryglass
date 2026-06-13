//! Platform-specific operations: clipboard, shell integration.
//!
//! Provides cross-platform abstractions that dispatch to native APIs
//! on Windows and fall back to simpler approaches on other platforms.

use std::path::Path;

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
// File associations ("Default apps" registration)
// ---------------------------------------------------------------------------

/// Register this exe as an "Open with" candidate for every supported
/// format, per-user (no admin). Windows requires the user to pick the
/// default themselves in Settings, applications can only volunteer.
pub fn register_file_associations() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    return windows::register_file_associations();

    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("file association registration is Windows-only for now");
}

/// Remove everything `register_file_associations` wrote.
pub fn unregister_file_associations() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    return windows::unregister_file_associations();

    #[cfg(not(target_os = "windows"))]
    anyhow::bail!("file association registration is Windows-only for now");
}

/// Whether this app is currently registered with the OS.
pub fn file_associations_registered() -> bool {
    #[cfg(target_os = "windows")]
    return windows::file_associations_registered();

    #[cfg(not(target_os = "windows"))]
    false
}

/// The ProgID groups we register: (progid, friendly name, extensions).
/// Extensions come from the live decoder registry, so new formats flow
/// through automatically. Plain archives (zip, 7z, rar) are left out; only
/// the comic variants are registered.
// Lives outside the windows module so the partition logic is tested on
// every platform.
#[cfg(any(target_os = "windows", test))]
pub fn association_groups() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    let images: Vec<&'static str> = crate::media::registry::global().extensions().collect();
    let videos: Vec<&'static str> = crate::video::EXTENSIONS.to_vec();
    let mut comics = vec!["cbz", "cb7"];
    if cfg!(feature = "rar") {
        comics.push("cbr");
    }

    let mut groups = vec![("scryglass.image", "Image", images)];
    if !videos.is_empty() {
        groups.push(("scryglass.video", "Video", videos));
    }
    groups.push(("scryglass.comic", "Comic book archive", comics));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn association_groups_cover_core_formats() {
        let groups = association_groups();
        let find = |ext: &str| {
            groups
                .iter()
                .find(|(_, _, exts)| exts.contains(&ext))
                .map(|(progid, _, _)| *progid)
        };

        assert_eq!(find("png"), Some("scryglass.image"));
        assert_eq!(find("cbz"), Some("scryglass.comic"));
        // Plain archives stay unclaimed on purpose.
        assert_eq!(find("zip"), None);
        assert_eq!(find("rar"), None);
        #[cfg(feature = "video")]
        assert_eq!(find("mp4"), Some("scryglass.video"));
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use std::path::Path;

    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// HKCU paths owned by the registration. Everything written lives
    /// under these, so unregistration is two subtree deletes plus one
    /// value.
    const CAPABILITIES: &str = r"Software\scryglass\Capabilities";
    const APP_ROOT: &str = r"Software\scryglass";
    const REGISTERED_APPS: &str = r"Software\RegisteredApplications";

    pub fn register_file_associations() -> anyhow::Result<()> {
        let exe = std::env::current_exe().context("locating the running exe")?;
        let exe = exe.display();
        let open_command = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);

        for (progid, friendly, extensions) in super::association_groups() {
            let (key, _) = hkcu
                .create_subkey(format!(r"Software\Classes\{progid}"))
                .with_context(|| format!("creating ProgID {progid}"))?;
            key.set_value("", &format!("scryglass {friendly}"))?;
            key.set_value("FriendlyTypeName", &format!("scryglass {friendly}"))?;
            let (icon_key, _) = key.create_subkey("DefaultIcon")?;
            icon_key.set_value("", &icon)?;
            let (cmd, _) = key.create_subkey(r"shell\open\command")?;
            cmd.set_value("", &open_command)?;

            let (assoc, _) = hkcu.create_subkey(format!(r"{CAPABILITIES}\FileAssociations"))?;
            for ext in extensions {
                assoc.set_value(format!(".{ext}"), &progid)?;
            }
        }

        let (caps, _) = hkcu.create_subkey(CAPABILITIES)?;
        caps.set_value("ApplicationName", &"scryglass")?;
        caps.set_value(
            "ApplicationDescription",
            &"A lightweight, blazing-fast image viewer",
        )?;

        let (registered, _) = hkcu.create_subkey(REGISTERED_APPS)?;
        registered.set_value("scryglass", &CAPABILITIES)?;

        notify_shell();
        Ok(())
    }

    pub fn file_associations_registered() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(REGISTERED_APPS)
            .and_then(|key| key.get_value::<String, _>("scryglass"))
            .is_ok()
    }

    pub fn unregister_file_associations() -> anyhow::Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);

        if let Ok(registered) =
            hkcu.open_subkey_with_flags(REGISTERED_APPS, winreg::enums::KEY_SET_VALUE)
        {
            let _ = registered.delete_value("scryglass");
        }
        let _ = hkcu.delete_subkey_all(APP_ROOT);
        for (progid, _, _) in super::association_groups() {
            let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{progid}"));
        }

        notify_shell();
        Ok(())
    }

    /// Tell Explorer the association set changed so menus refresh
    /// without a logoff.
    fn notify_shell() {
        use windows_sys::Win32::UI::Shell;
        const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
        unsafe {
            Shell::SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null());
        }
    }

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
