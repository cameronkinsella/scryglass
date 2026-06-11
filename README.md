# scryglass

A lightweight, blazing-fast image viewer built with [iced](https://github.com/iced-rs/iced) 0.14.

## Features

- **Instant navigation**: arrow keys, A/D keys, or mouse back/forward buttons flip through images, hold to scroll continuously, and the image area keeps up with blurred placeholders while full decodes finish
- **Flicker-free display**: images are pre-allocated as GPU textures before being shown, so navigation never blocks on I/O
- **Pre-fetching & caching**: neighboring images decode in the background into an LRU cache with a byte budget, and stale loads cancel when you move on
- **Archive browsing**: open zip/cbz, tar, tar.gz, 7z/cb7, and rar/cbr files like folders, and animations inside archives play
- **Zoom modes**: Auto, Lock Zoom Ratio, Scale to Width/Height/Fit/Fill, plus scroll-wheel zoom toward the cursor, drag-to-pan, and an optional crisp-pixels mode for pixel art past 100%
- **Animation support**: GIF (with proper disposal-method compositing), APNG, and animated WebP play at their native frame rate
- **EXIF aware**: orientation is applied, embedded camera thumbnails power instant previews, and `I` opens an info panel with camera metadata
- **Persistent thumbnails, done right**: folders open warm across sessions, and cached thumbnails of deleted files are purged when their folder reopens, unused entries expire after 90 days, and the store is capped at 512 MB, all judged from local metadata, so sleeping external drives are never touched. Disable via Settings or build without the `disk-thumbs` feature
- **File management**: delete to recycle bin and rename in place, with a read-only mode that hides and blocks both
- **Video playback** (`--features video`): a full player linked against FFmpeg's libraries (in-process demux and decode, no external processes): play/pause, seek bar plus J/L ±10s, volume, mute, and loop, with auto-hiding controls. Audio is the playback clock. Videos get first-frame filmstrip thumbnails and play from inside archives too (extracted to a self-cleaning temp file)
- **Sorting**: natural name (default), plain name, date modified, or size, ascending or descending, with metadata fetched off-thread
- **View rotation**: `R`/`Shift+R` rotate in quarter turns without touching the file
- **Fullscreen**: `F`/`F11` hides all chrome
- **"Open with…" support**: pass a file, folder, or archive as a CLI argument
- **Drag-and-drop**: drop any image, folder, or archive to open it, and navigation wraps around
- **Filmstrip, slider, footer, info panel**: virtualized thumbnail strip, position slider with live scrubbing, info footer, and EXIF panel, each individually toggleable
- **Context menu**: copy image (bitmap) / file / path / filename, reveal in file manager, file properties, rename, delete
- **Dark & light themes**: near-black chrome designed for photo viewing (default), light theme optional
- **Persistent settings**: zoom mode, sort, layout, theme, window size, and more are saved to `config.toml` and restored on launch

## Supported Formats

PNG, JPEG, GIF, APNG, BMP, WebP (incl. animated), TIFF, ICO, AVIF, JPEG XL, SVG, and camera RAW embedded previews (CR2/CR3/NEF/ARW/DNG/ORF/RW2/RAF/PEF/SRW), plus zip, cbz, tar, tar.gz, tgz, 7z, cb7, rar, and cbr archives containing them. HEIC/HEIF is available behind the `heif` feature (needs system libheif), and video containers (MP4, MKV, WebM, MOV, AVI, M4V) behind the `video` feature.

## Keyboard & Mouse

| Input | Action |
|---|---|
| `→` / `D`, `←` / `A` | Next / previous image (hold to scroll) |
| `Home` / `End` | First / last image |
| Scroll wheel, `+` / `−` | Zoom (toward cursor / center) |
| `Ctrl+0`, double-click | Reset zoom |
| `Ctrl+1` | Zoom to 100% |
| Left-drag | Pan (when zoomed in) |
| `F` / `F11` | Fullscreen |
| `I` | Info panel |
| `R` / `Shift+R` | Rotate view |
| `Delete`, `F2` | Recycle / rename |
| `Space`, `J` / `L`, `M`, `↑` / `↓` | Video: play/pause, seek, mute, volume |
| `?` | Shortcut help |
| Right-click | Context menu |
| `Esc` | Close dialogs / leave fullscreen / dismiss menus |

## Build & Run

Requires Rust edition 2024.

```
cargo build                    # compile (default features)
cargo run -- <path>            # open a file, folder, or archive
cargo build --features video   # include video playback (see below)
cargo build --features heif    # include HEIC/HEIF (needs system libheif)
cargo test                     # run tests
```

Building with `video` links FFmpeg's libraries. On Linux, install the dev
packages (`libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
libswresample-dev`) plus `clang`. On Windows, install LLVM and an FFmpeg
7.x *shared* build (e.g. `winget install LLVM.LLVM Gyan.FFmpeg.Shared`),
then set `FFMPEG_DIR` to the FFmpeg folder containing `include/` and
`lib/`, and `LIBCLANG_PATH` to LLVM's `bin` directory. At runtime the
FFmpeg DLLs must be on PATH or beside the executable (LGPL dynamic
linking).

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

Images are loaded through iced's `image::allocate()` API, which returns GPU-resident `Allocation` objects. Holding an `Allocation` guarantees the texture renders immediately: no decode delay, no flicker. All I/O (directory scans, decodes, metadata, config writes) runs off the UI thread, and every load is cancellable the moment you navigate away.

## License

MIT
