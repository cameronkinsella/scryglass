# scryglass

[![CI](https://github.com/cameronkinsella/scryglass/actions/workflows/ci.yml/badge.svg)](https://github.com/cameronkinsella/scryglass/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/scryglass.svg)](https://crates.io/crates/scryglass)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/cameronkinsella/scryglass/blob/master/LICENSE)

A lightweight, blazing-fast image viewer built with [iced](https://github.com/iced-rs/iced) 0.14.

## Install

Prebuilt binaries are attached to each [release](https://github.com/cameronkinsella/scryglass/releases),
self-contained with no runtime dependencies. The quickest path is
[cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```
cargo binstall scryglass
```

Or build from source (`cargo install scryglass`), see Build & Run below
for the optional native features.

Every release ships two downloads per platform, both with all features
(video playback on statically linked FFmpeg, AV1/AVIF, and HEIC/HEIF):

- An OS application: a Windows installer (`-setup.exe`), a Linux
  `AppImage`, or a macOS `.dmg`.
- A slim archive holding just the binary: a `.zip` on Windows, a
  `.tar.gz` on Linux and macOS. This is what `cargo binstall` fetches.

The macOS app is unsigned, so Gatekeeper blocks the first launch. Either
right-click the app and pick **Open** (macOS 14 and earlier), or open
**System Settings > Privacy & Security** and click **Open Anyway**
(macOS 15 Sequoia removed the right-click path). If it still refuses,
clear quarantine from a terminal:

```
xattr -dr com.apple.quarantine /Applications/scryglass.app
```

The `.dmg` is the cleanest macOS install (drag the app out before
launching). The slim `.tar.gz` suits `cargo binstall` and scripting.

To make scryglass your default viewer on Windows, install it (the
installer puts it at a stable path), then turn on file associations in
its Settings and pick it under Settings > Apps > Default apps. This
registers every supported image, video, and comic format for the
current user, no admin needed.

## Uninstall

How you remove scryglass depends on how it was installed:

- **Windows installer**: run *Uninstall scryglass* from the Start menu or
  Settings > Apps. It offers to also delete your settings and thumbnail
  cache. Choose No to keep them for a reinstall.
- **macOS**: drag `scryglass.app` to the Trash.
- **Linux AppImage** or a **portable archive**: delete the file or folder.

Apart from the Windows installer's prompt, removal leaves your data in the
standard per-user locations, all safe to delete by hand:

| Data                      | Windows                    | Linux                 | macOS                                    |
|---------------------------|----------------------------|-----------------------|------------------------------------------|
| Settings (`config.toml`)  | `%APPDATA%\scryglass`      | `~/.config/scryglass` | `~/Library/Application Support/scryglass` |
| Thumbnail cache           | `%LOCALAPPDATA%\scryglass` | `~/.cache/scryglass`  | `~/Library/Caches/scryglass`             |

The thumbnail cache is the larger of the two and can also be cleared anytime
from File → Settings.

## Features

- **Smooth navigation**: every keypress moves immediately, blurred placeholders cover slow loads, and
  images render as pre-allocated GPU textures
- **Built for slow storage**: all I/O runs off-thread with cancellation, so stalled reads never freeze the UI
- **Smart caching**: background prefetch into a byte-budgeted LRU cache, plus persistent disk thumbnails with
  deleted-file purging, 90-day expiry, and a 512 MB cap
- **Archives as folders**: browse zip/cbz, tar, 7z/cb7, and rar/cbr directly, including the animations and videos inside
- **Animation playback**: GIF, APNG, and animated WebP at native frame rate with correct frame compositing
- **Video player** (`video` feature): in-process FFmpeg decode with audio-synced playback, seeking, volume, looping, and
  auto-hiding controls
- **Flexible zoom**: six zoom modes, scroll-wheel zoom toward the cursor, drag-to-pan, and a crisp-pixels mode for pixel
  art
- **EXIF aware**: orientation applied, embedded thumbnails for instant previews, and an info panel with camera metadata
- **File management**: recycle-bin delete and in-place rename, with an optional read-only mode
- **Native sorting**: files order exactly like your file manager, with date and size options
- **Comfortable**: dark and light themes, a virtualized filmstrip, position slider, context menu, persistent settings,
  and a full keyboard map (`?` shows it)

## Supported Formats

| Type       | Formats                                                                 |
|------------|-------------------------------------------------------------------------|
| Images     | PNG, JPEG, GIF, BMP, WebP, TIFF, ICO, JPEG XL, SVG                      |
| Animations | GIF, APNG, animated WebP                                                |
| Camera RAW | embedded previews from CR2, CR3, NEF, ARW, DNG, ORF, RW2, RAF, PEF, SRW |
| HEIC/HEIF  | `heif` feature                                                          |
| AVIF       | `video` feature (decoded through FFmpeg)                                |
| Video      | MP4, MKV, WebM, MOV, AVI, M4V including AV1 (`video` feature)           |
| Archives   | zip, cbz, tar, tar.gz, tgz, 7z, cb7, rar, cbr                           |

## Keyboard & Mouse

| Input                              | Action                                           |
|------------------------------------|--------------------------------------------------|
| `→` / `D`, `←` / `A`               | Next / previous image (hold to scroll)           |
| `Home` / `End`                     | First / last image                               |
| Scroll wheel, `+` / `−`            | Zoom (toward cursor / center)                    |
| `Ctrl+0`, double-click             | Reset zoom                                       |
| `Ctrl+1`                           | Zoom to 100%                                     |
| Left-drag                          | Pan (when zoomed in)                             |
| `F` / `F11`                        | Fullscreen                                       |
| `I`                                | Info panel                                       |
| `R` / `Shift+R`                    | Rotate view                                      |
| `Delete`, `F2`                     | Recycle / rename                                 |
| `Space`, `J` / `L`, `M`, `↑` / `↓` | Video: play/pause, seek, mute, volume            |
| `?`                                | Shortcut help                                    |
| Right-click                        | Context menu                                     |
| `Esc`                              | Close dialogs / leave fullscreen / dismiss menus |

## Build & Run

Requires a current stable Rust toolchain with Rust 2024 edition support.
The default feature set is pure Rust except for the optional RAR support,
which builds vendored unrar C++ sources.

```
cargo build                         # default image/archive viewer
cargo run -- <path>                 # open a file, folder, or archive
cargo build --no-default-features   # smallest dependency surface
cargo test                          # run tests
```

Advanced native builds (`video`, `heif`, and static FFmpeg/media
linking) are covered in [BUILD.md](BUILD.md).

## Configuration

Settings persist to `config.toml` in the platform config directory
(`%APPDATA%\scryglass` on Windows, `~/.config/scryglass` on Linux,
`~/Library/Application Support/scryglass` on macOS). Unknown keys are
ignored, so configs survive upgrades in both directions. Most settings
are editable in-app via File → Settings.

## Architecture

scryglass follows the Elm Architecture (iced pattern):

- **`App`**: single source of truth for all state
- **`Message`**: enum driving all state transitions
- **`update()`**: handles messages, fires async tasks
- **`view()`**: pure rendering, no side effects

Images are loaded through iced's `image::allocate()` API, which returns GPU-resident `Allocation` objects. Holding an
`Allocation` guarantees the texture renders immediately: no decode delay, no flicker. All I/O (directory scans, decodes,
metadata, config writes) runs off the UI thread, and every load is cancellable the moment you navigate away.

## License

MIT
