//! Platform-specific operations: clipboard, shell integration.

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

/// Open the file's parent folder in the native file manager and, on Windows,
/// select the file. Other platforms only open the folder.
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

/// Open the native file properties dialog for the given path. A no-op off
/// Windows.
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

// ---------------------------------------------------------------------------
// Present-mode support probe
// ---------------------------------------------------------------------------

/// The explicit wgpu present modes the GPU driver offers, queried against an
/// invisible throwaway window before iced boots. iced treats configuring a
/// mode the driver lacks as fatal, and drivers differ (AMD's Windows Vulkan
/// driver has no mailbox), so a `[startup]` mode that can be missing is
/// verified here first. `None` means the probe failed and nothing can be
/// assumed. The answer is cached against the graphics identity, so every
/// launch after the first is a file read.
#[cfg(target_os = "windows")]
pub fn supported_present_modes() -> Option<Vec<crate::config::PresentMode>> {
    windows::supported_present_modes()
}

/// A cached probe answer, valid while the graphics identity (every display
/// adapter's name and driver version) is unchanged. A new GPU or driver
/// re-probes, as does a stale or unreadable cache, so no wrong mode can
/// outlive the driver that reported it.
// Lives outside the windows module so the hit rule is tested on every
// platform.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct PresentProbeCache {
    graphics_identity: String,
    modes: Vec<crate::config::PresentMode>,
}

/// The cached modes, if the cache was written under the same identity.
#[cfg(any(target_os = "windows", test))]
fn cached_present_modes(
    identity: Option<&str>,
    cache: Option<PresentProbeCache>,
) -> Option<Vec<crate::config::PresentMode>> {
    let cache = cache?;
    (identity? == cache.graphics_identity).then_some(cache.modes)
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

/// The registered ProgID groups: (progid, friendly name, extensions).
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

    #[test]
    fn no_extension_is_claimed_by_two_groups() {
        let mut seen = std::collections::HashSet::new();
        for (_, _, exts) in association_groups() {
            for ext in exts {
                assert!(seen.insert(ext), "{ext} is claimed by more than one ProgID");
            }
        }
    }

    #[test]
    fn probe_cache_hits_only_on_matching_identity() {
        use crate::config::PresentMode;
        let cache = || {
            Some(PresentProbeCache {
                graphics_identity: "gpu 1.0".into(),
                modes: vec![PresentMode::Mailbox],
            })
        };
        assert_eq!(
            cached_present_modes(Some("gpu 1.0"), cache()),
            Some(vec![PresentMode::Mailbox])
        );
        assert_eq!(cached_present_modes(Some("gpu 2.0"), cache()), None);
        assert_eq!(cached_present_modes(None, cache()), None);
        assert_eq!(cached_present_modes(Some("gpu 1.0"), None), None);
    }

    #[test]
    fn probe_cache_toml_round_trips() {
        use crate::config::PresentMode;
        let cache = PresentProbeCache {
            graphics_identity: "NVIDIA X 32.0.15\nIntel Y 31.0".into(),
            modes: vec![PresentMode::FifoRelaxed, PresentMode::Mailbox],
        };
        let text = toml::to_string(&cache).unwrap();
        assert_eq!(toml::from_str::<PresentProbeCache>(&text).unwrap(), cache);
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use anyhow::Context;
    use windows_sys::Win32::Foundation;
    use windows_sys::Win32::System::Com;
    use windows_sys::Win32::System::LibraryLoader;
    use windows_sys::Win32::UI::Shell;
    use windows_sys::Win32::UI::WindowsAndMessaging;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

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

    /// A null-terminated UTF-16 copy of `s`, as the wide-string Win32 APIs
    /// expect.
    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Initializes COM for this thread and uninitializes it on drop, but only
    /// when this guard was the one that initialized it.
    struct ComGuard {
        owns: bool,
    }

    impl ComGuard {
        fn apartment_threaded() -> Self {
            // SAFETY: CoInitializeEx is always safe to call with a null
            // reserved pointer. A success code (>= 0) means the call took a
            // reference that drop must release. S_FALSE (already initialized
            // on this thread) still counts and is balanced too.
            let hr = unsafe {
                Com::CoInitializeEx(std::ptr::null(), Com::COINIT_APARTMENTTHREADED as u32)
            };
            Self { owns: hr >= 0 }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.owns {
                // SAFETY: balanced against this guard's successful
                // CoInitializeEx on the same thread.
                unsafe { Com::CoUninitialize() };
            }
        }
    }

    /// Owns a shell ITEMIDLIST and frees it with CoTaskMemFree on drop.
    struct Pidl(*mut Shell::Common::ITEMIDLIST);

    impl Drop for Pidl {
        fn drop(&mut self) {
            // SAFETY: the pointer came from SHParseDisplayName and is freed
            // exactly once, here.
            unsafe { Com::CoTaskMemFree(self.0 as *const _) };
        }
    }

    /// Tell Explorer the association set changed so menus refresh
    /// without a logoff.
    fn notify_shell() {
        const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
        // SAFETY: SHChangeNotify takes scalars and null item pointers, so
        // there is nothing to keep alive across the call.
        unsafe {
            Shell::SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null());
        }
    }

    /// Reveal a file in Explorer, reusing an existing window if possible.
    /// Uses `SHOpenFolderAndSelectItems`, which highlights the file.
    pub fn reveal_in_file_manager(path: &Path) {
        // The shell call needs COM up for its duration.
        let _com = ComGuard::apartment_threaded();

        let wide = to_wide(path.as_os_str());
        let mut pidl: *mut Shell::Common::ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: `wide` is a null-terminated UTF-16 string that lives past
        // the call; on success (hr == 0) SHParseDisplayName writes an owned
        // PIDL into `pidl`, which the Pidl guard then frees.
        let hr = unsafe {
            Shell::SHParseDisplayName(
                wide.as_ptr(),
                std::ptr::null_mut(),
                &mut pidl,
                0,
                std::ptr::null_mut(),
            )
        };
        if hr != 0 || pidl.is_null() {
            return;
        }
        let pidl = Pidl(pidl);
        // SAFETY: `pidl.0` is the valid PIDL just parsed; passing zero items
        // opens or reuses the folder window and selects the parsed item.
        unsafe {
            Shell::SHOpenFolderAndSelectItems(pidl.0, 0, std::ptr::null(), 0);
        }
    }

    /// Open the Windows Properties dialog via `ShellExecuteExW` with the
    /// `"properties"` verb and `SEE_MASK_INVOKEIDLIST`.
    pub fn show_properties(path: &Path) {
        const SEE_MASK_INVOKEIDLIST: u32 = 0x0000_000C;

        let verb: Vec<u16> = "properties\0".encode_utf16().collect();
        let file = to_wide(path.as_os_str());

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
            // SAFETY: the anonymous union is a handle field unused by the
            // "properties" verb; zeroed is the documented "none" value.
            Anonymous: unsafe { std::mem::zeroed() },
            hProcess: std::ptr::null_mut(),
        };

        // SAFETY: `info` is fully initialized with cbSize set, and `verb` and
        // `file` are null-terminated strings that outlive the call.
        unsafe {
            Shell::ShellExecuteExW(&mut info);
        }
    }

    /// An invisible 1x1 window that exists only to give wgpu a surface to
    /// query driver capabilities against. Torn down when the probe returns.
    struct ProbeWindow {
        hwnd: Foundation::HWND,
        class: Vec<u16>,
        hinstance: Foundation::HINSTANCE,
    }

    impl ProbeWindow {
        fn create() -> Option<Self> {
            let class: Vec<u16> = "scryglass-present-probe\0".encode_utf16().collect();
            // SAFETY: plain window-class registration and creation with a
            // null-terminated class name that outlives the window. Drop
            // tears both down.
            unsafe {
                let hinstance = LibraryLoader::GetModuleHandleW(std::ptr::null());
                let wc = WindowsAndMessaging::WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(WindowsAndMessaging::DefWindowProcW),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinstance,
                    hIcon: std::ptr::null_mut(),
                    hCursor: std::ptr::null_mut(),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: class.as_ptr(),
                };
                if WindowsAndMessaging::RegisterClassW(&wc) == 0 {
                    return None;
                }
                let hwnd = WindowsAndMessaging::CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    0, // not WS_VISIBLE: the window is never shown
                    0,
                    0,
                    1,
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    hinstance,
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    WindowsAndMessaging::UnregisterClassW(class.as_ptr(), hinstance);
                    return None;
                }
                Some(Self {
                    hwnd,
                    class,
                    hinstance,
                })
            }
        }
    }

    impl Drop for ProbeWindow {
        fn drop(&mut self) {
            // SAFETY: both handles were created in `create` and nothing uses
            // them after this.
            unsafe {
                WindowsAndMessaging::DestroyWindow(self.hwnd);
                WindowsAndMessaging::UnregisterClassW(self.class.as_ptr(), self.hinstance);
            }
        }
    }

    pub fn supported_present_modes() -> Option<Vec<crate::config::PresentMode>> {
        let identity = graphics_identity();
        if let Some(modes) = super::cached_present_modes(identity.as_deref(), load_probe_cache()) {
            #[cfg(debug_assertions)]
            eprintln!("present-mode probe: cache hit {modes:?}");
            return Some(modes);
        }
        #[cfg(debug_assertions)]
        let start = std::time::Instant::now();
        let window = ProbeWindow::create()?;
        let modes = query_present_modes(&window)?;
        #[cfg(debug_assertions)]
        eprintln!("present-mode probe: {modes:?} in {:.1?}", start.elapsed());
        if let Some(graphics_identity) = identity {
            save_probe_cache(&super::PresentProbeCache {
                graphics_identity,
                modes: modes.clone(),
            });
        }
        Some(modes)
    }

    /// Every display adapter's name and driver version from the display
    /// class registry key, one sorted line each. Any change to the machine's
    /// graphics setup (a new GPU, a driver update) changes this string and
    /// invalidates the probe cache. `None` disables caching, never the probe.
    fn graphics_identity() -> Option<String> {
        // The display adapter setup class, fixed across Windows versions.
        let class = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(
                r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}",
            )
            .ok()?;
        let mut lines: Vec<String> = class
            .enum_keys()
            .flatten()
            .filter_map(|name| {
                let adapter = class.open_subkey(&name).ok()?;
                let desc: String = adapter.get_value("DriverDesc").ok()?;
                let version: String = adapter.get_value("DriverVersion").unwrap_or_default();
                Some(format!("{desc} {version}"))
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        lines.sort();
        Some(lines.join("\n"))
    }

    /// `<exe>/data/present-probe.toml` in a portable install, else under the
    /// OS cache dir (the disk-thumbs convention).
    fn probe_cache_path() -> Option<std::path::PathBuf> {
        match crate::config::data_dir() {
            crate::config::DataDir::Portable(data) => Some(data.join("present-probe.toml")),
            _ => Some(
                dirs::cache_dir()?
                    .join("scryglass")
                    .join("present-probe.toml"),
            ),
        }
    }

    fn load_probe_cache() -> Option<super::PresentProbeCache> {
        let text = std::fs::read_to_string(probe_cache_path()?).ok()?;
        toml::from_str(&text).ok()
    }

    /// Best-effort: a failed write means the next launch probes again,
    /// and a torn write fails to parse, which does the same.
    fn save_probe_cache(cache: &super::PresentProbeCache) {
        let Some(path) = probe_cache_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(text) = toml::to_string(cache) else {
            return;
        };
        let _ = std::fs::write(path, text);
    }

    fn query_present_modes(window: &ProbeWindow) -> Option<Vec<crate::config::PresentMode>> {
        use crate::config::PresentMode;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            // Unlike iced, GL is left out: its instance setup (a dummy WGL
            // context) is the slow part of a probe, and the hardware-GPU
            // check below covers the machines where iced would pick it.
            backends: wgpu::Backends::from_env()
                .unwrap_or(wgpu::Backends::VULKAN | wgpu::Backends::DX12),
            // iced boots with empty flags. The wgpu default turns on
            // validation in debug builds, which the real surface never
            // runs with.
            flags: wgpu::InstanceFlags::empty(),
            ..Default::default()
        });
        let mut handle =
            wgpu::rwh::Win32WindowHandle::new(std::num::NonZeroIsize::new(window.hwnd as isize)?);
        // Vulkan refuses a Win32 handle without its hinstance (wgpu-hal
        // vulkan/instance.rs). Leaving it unset silently drops the Vulkan
        // adapters and skews the probe to another backend's answer.
        handle.hinstance = std::num::NonZeroIsize::new(window.hinstance as isize);
        // SAFETY: the raw handle points at the live probe window, which
        // outlives the surface. Both end with this function.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: wgpu::rwh::RawDisplayHandle::Windows(
                    wgpu::rwh::WindowsDisplayHandle::new(),
                ),
                raw_window_handle: wgpu::rwh::RawWindowHandle::Win32(handle),
            })
        }
        .ok()?;
        // The same request iced_wgpu makes (window/compositor.rs), so the
        // probed adapter is the one the real surface will run on.
        let adapter = tokio::runtime::Builder::new_current_thread()
            .build()
            .ok()?
            .block_on(
                instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::from_env()
                        .unwrap_or(wgpu::PowerPreference::HighPerformance),
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }),
            )
            .ok()?;
        let info = adapter.get_info();
        #[cfg(debug_assertions)]
        eprintln!(
            "present-mode probe adapter: {} ({:?})",
            info.name, info.backend
        );
        // iced picks across every backend including GL, which this probe
        // skips. A hardware GPU always outranks GL in that pick (wgpu sorts
        // by device type), so only a hardware winner here is guaranteed to
        // be the adapter the real surface runs on.
        if !matches!(
            info.device_type,
            wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
        ) {
            return None;
        }
        let modes = surface.get_capabilities(&adapter).present_modes;
        Some(
            modes
                .iter()
                .filter_map(|mode| match mode {
                    wgpu::PresentMode::Mailbox => Some(PresentMode::Mailbox),
                    wgpu::PresentMode::FifoRelaxed => Some(PresentMode::FifoRelaxed),
                    wgpu::PresentMode::Immediate => Some(PresentMode::Immediate),
                    // The guaranteed modes never consult the probe.
                    _ => None,
                })
                .collect(),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn to_wide_appends_a_null_terminator() {
            assert_eq!(to_wide(OsStr::new("ab")), vec![0x61, 0x62, 0x00]);
        }

        #[test]
        fn to_wide_of_empty_is_just_the_terminator() {
            assert_eq!(to_wide(OsStr::new("")), vec![0x00]);
        }
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
