# Building scryglass

Most contributors only need the default build:

```
cargo build
```

This document covers optional native features and static FFmpeg/media
builds. The release workflow in `.github/workflows/release.yml` is the
source of truth for official binaries.

## Optional Features

```
cargo build --features heif       # HEIC/HEIF
cargo build --features video      # video + AVIF
cargo build --all-features        # every optional feature
```

The `video-static` feature is included by `--all-features`. It asks
`ffmpeg-sys-next` to link FFmpeg statically.

Feature requirements:

| Feature        | Adds                                | Requires                                           |
|----------------|-------------------------------------|----------------------------------------------------|
| `rar`          | RAR/CBR archive browsing            | C++ toolchain for vendored unrar sources           |
| `disk-thumbs`  | Persistent thumbnail cache          | No native library                                  |
| `jxl`          | JPEG XL decoding                    | No native library                                  |
| `svg`          | SVG rendering                       | No native library                                  |
| `raw`          | Camera RAW embedded previews        | No native library                                  |
| `heif`         | HEIC/HEIF decoding                  | `libheif` headers and libraries                    |
| `video`        | Video playback and AVIF decoding    | FFmpeg dev libraries, libclang, audio system libs  |
| `video-static` | Static FFmpeg link for `video`      | Static FFmpeg libraries; use vcpkg instructions below |

## Shared Native Builds

Use these for local development when native libraries can stay installed
on the machine.

Linux:

```
sudo apt-get install clang pkg-config libgtk-3-dev libasound2-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev \
  libswresample-dev libheif-dev
cargo build --features video,heif
```

Windows:

```
winget install LLVM.LLVM Gyan.FFmpeg.Shared
$env:FFMPEG_DIR = "C:\path\to\ffmpeg"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --features video
```

`FFMPEG_DIR` must contain `include/` and `lib/`. With a shared FFmpeg
build, the FFmpeg DLLs must be on `PATH` or beside the executable at
runtime. For `heif` on Windows, the vcpkg path below is the better-tested
route.

macOS:

```
brew install llvm pkg-config ffmpeg libheif
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
cargo build --features video,heif
```

## Static FFmpeg/Media Builds

Official releases build static FFmpeg and libheif through
[vcpkg](https://github.com/microsoft/vcpkg). To reproduce that path
locally, install vcpkg first and point `VCPKG_ROOT` at the checkout.

Windows:

```
git clone https://github.com/microsoft/vcpkg C:\path\to\vcpkg
C:\path\to\vcpkg\bootstrap-vcpkg.bat
$env:VCPKG_ROOT = "C:\path\to\vcpkg"
```

Linux/macOS:

```
git clone https://github.com/microsoft/vcpkg /path/to/vcpkg
/path/to/vcpkg/bootstrap-vcpkg.sh
export VCPKG_ROOT="/path/to/vcpkg"
```

The release workflow pins vcpkg with `VCPKG_COMMIT`. Local builds can use
the current vcpkg checkout, but checking out the workflow's pinned commit
is the closest match to release artifacts.

### Windows

Prerequisites:

- Visual Studio Build Tools or Visual Studio with the MSVC toolchain
- Windows SDK 10.0.22000 or newer
- LLVM/libclang, for example `winget install LLVM.LLVM`

Build:

```
& "$env:VCPKG_ROOT\vcpkg.exe" install `
  ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-windows-static-md `
  libheif[core]:x64-windows-static-md `
  --overlay-triplets="$PWD\.github\vcpkg-triplets"
$env:FFMPEG_DIR = "$env:VCPKG_ROOT\installed\x64-windows-static-md"
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release --all-features
```

`VCPKG_ROOT` is needed by `libheif-sys`; `FFMPEG_DIR` is needed by
`ffmpeg-sys-next`. The build script rejects a shared Windows FFmpeg build
when `video-static` is enabled, so `FFMPEG_DIR` must point at the static
triplet.

### Linux

Prerequisites:

```
sudo apt-get install clang pkg-config nasm libasound2-dev libgtk-3-dev
```

Build:

```
"$VCPKG_ROOT/vcpkg" install \
  'ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-linux' \
  'libheif[core]:x64-linux' \
  --overlay-triplets="$(pwd)/.github/vcpkg-triplets"
export MEDIA_PREFIX="$VCPKG_ROOT/installed/x64-linux"
export PKG_CONFIG_PATH="$MEDIA_PREFIX/lib/pkgconfig"
export PKG_CONFIG_ALL_STATIC=1
cargo build --release --all-features --target x86_64-unknown-linux-gnu
```

### macOS

The release builds use custom vcpkg triplets from
`.github/vcpkg-triplets` so the static libraries use the same macOS
deployment target as the Rust binary.

Prerequisites:

```
brew install pkg-config
```

Apple Silicon:

```
"$VCPKG_ROOT/vcpkg" install \
  'ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:arm64-osx' \
  'libheif[core]:arm64-osx' \
  --overlay-triplets="$(pwd)/.github/vcpkg-triplets"
export MEDIA_PREFIX="$VCPKG_ROOT/installed/arm64-osx"
export PKG_CONFIG_PATH="$MEDIA_PREFIX/lib/pkgconfig"
export PKG_CONFIG_ALL_STATIC=1
export MACOSX_DEPLOYMENT_TARGET=11.0
unset FFMPEG_DIR
cargo build --release --all-features --target aarch64-apple-darwin
```

Intel:

```
brew install nasm
"$VCPKG_ROOT/vcpkg" install \
  'ffmpeg[core,avcodec,avformat,swscale,swresample,dav1d]:x64-osx' \
  'libheif[core]:x64-osx' \
  --overlay-triplets="$(pwd)/.github/vcpkg-triplets"
export MEDIA_PREFIX="$VCPKG_ROOT/installed/x64-osx"
export PKG_CONFIG_PATH="$MEDIA_PREFIX/lib/pkgconfig"
export PKG_CONFIG_ALL_STATIC=1
export MACOSX_DEPLOYMENT_TARGET=11.0
rtlib="$(clang --print-resource-dir)/lib/darwin/libclang_rt.osx.a"
export RUSTFLAGS="-C link-arg=$rtlib"
unset FFMPEG_DIR
cargo build --release --all-features --target x86_64-apple-darwin
```

On macOS, do not force `FFMPEG_DIR`; the release build uses pkg-config
metadata so static dependencies such as `dav1d` are discovered.

## Licensing

The static FFmpeg configuration is LGPL-clean: no x264 or x265 is
included, HEVC decoding uses FFmpeg's native LGPL decoder, and AV1 uses
dav1d, which is BSD-2. LGPL's relink requirement is satisfied by this
project being open source.
