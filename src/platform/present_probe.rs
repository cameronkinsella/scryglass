//! Present-mode support probe.

/// The explicit wgpu present modes the GPU driver offers, queried against an
/// invisible throwaway window before iced boots. iced treats configuring a
/// mode the driver lacks as fatal, and drivers differ (AMD's Windows Vulkan
/// driver has no mailbox), so an `[advanced.startup]` mode that can be
/// missing is verified here first. `None` means the probe failed and nothing
/// can be assumed. The answer is cached against the graphics identity, so every
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

#[cfg(test)]
mod tests {
    use super::*;

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
    use windows_sys::Win32::Foundation;
    use windows_sys::Win32::System::LibraryLoader;
    use windows_sys::Win32::UI::WindowsAndMessaging;
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

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
}
