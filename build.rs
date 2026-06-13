//! Windows resources (icon, version info) and build-time guards plus
//! link flags for the static-FFmpeg path.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");

    #[cfg(windows)]
    if windows {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "scryglass");
        res.set("FileDescription", "scryglass image viewer");
        if let Err(e) = res.compile() {
            println!("cargo:warning=resource embedding failed: {e}");
        }
    }

    let video_static = env::var("CARGO_FEATURE_VIDEO_STATIC").is_ok();

    // FFmpeg's dav1d decoder needs libdav1d on the link line too. The
    // bindings only list FFmpeg's own libraries when FFMPEG_DIR is set,
    // not their external dependencies.
    if video_static {
        println!("cargo:rustc-link-lib=static=dav1d");
    }

    if !video_static || !windows {
        return;
    }

    // `static=avcodec` and an import library are indistinguishable to
    // MSVC's linker. Pointing FFMPEG_DIR at a *shared* FFmpeg build
    // would silently produce a DLL-dependent exe. Catch it here.
    if let Ok(dir) = env::var("FFMPEG_DIR") {
        let bin = PathBuf::from(&dir).join("bin");
        let has_dlls = std::fs::read_dir(&bin).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                name.starts_with("avcodec") && name.ends_with(".dll")
            })
        });
        if has_dlls {
            panic!(
                "video-static requires a STATIC FFmpeg build, but FFMPEG_DIR \
                 ({dir}) contains FFmpeg DLLs (a shared build). Point it at a \
                 static package, e.g. vcpkg's installed/x64-windows-static-md."
            );
        }
    }

    // System libraries the static FFmpeg objects pull in that the
    // bindings' build script doesn't emit: schannel TLS, networking,
    // and the MediaFoundation/codec-API GUID repositories.
    println!("cargo:rustc-link-lib=dylib=ws2_32");
    println!("cargo:rustc-link-lib=dylib=secur32");
    println!("cargo:rustc-link-lib=dylib=crypt32");
    println!("cargo:rustc-link-lib=dylib=mfplat");
    println!("cargo:rustc-link-lib=dylib=mfuuid");
    println!("cargo:rustc-link-lib=dylib=uuid");
    println!("cargo:rustc-link-lib=dylib=strmiids");
    println!("cargo:rustc-link-lib=dylib=wmcodecdspuuid");
}
