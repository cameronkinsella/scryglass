# scryglass

A lightweight, blazing-fast image viewer built with [iced](https://github.com/iced-rs/iced) 0.14.

## Features

- **Instant navigation**: arrow keys, A/D keys, or mouse back/forward buttons flip through images, hold to scroll continuously, and the image area keeps up with blurred placeholders while full decodes finish
- **Flicker-free display**: images are pre-allocated as GPU textures before being shown, so navigation never blocks on I/O
- **Pre-fetching & caching**: neighboring images decode in the background into an LRU cache with a byte budget, and stale loads cancel when you move on
- **Archive browsing**: open zip/cbz, tar, tar.gz, 7z/cb7, and rar/cbr files like folders
- **Zoom modes**: Auto, Lock Zoom Ratio, Scale to Width/Height/Fit/Fill, plus scroll-wheel zoom toward the cursor, drag-to-pan, and an optional pixelated mode for crisp pixel art past 100%
- **Animated GIF support**: frames are decoded with proper disposal-method compositing and animated at their native frame rate
- **EXIF aware**: orientation is applied, and embedded camera thumbnails power instant previews
- **Persistent thumbnails, done right**: folders open warm across sessions, and cached thumbnails of deleted files are purged when their folder reopens, unused entries expire after 90 days, and the store is capped at 512 MB, all judged from local metadata, so sleeping external drives are never touched. Disable via the `disk_thumbs` config setting or build without the `disk-thumbs` feature
- **"Open with…" support**: pass a file, folder, or archive as a CLI argument
- **Drag-and-drop**: drop any image, folder, or archive to open it, and navigation wraps around
- **Natural sorting**: `img2` comes before `img10`, like a file manager
- **Filmstrip, slider, footer**: virtualized thumbnail strip that follows the current image, position slider, and an info footer, each individually toggleable
- **Context menu**: copy image / path / filename, reveal in file manager, file properties
- **Dark & light themes**: near-black chrome designed for photo viewing (default), light theme optional
- **Persistent settings**: zoom mode, layout, and theme are saved to `config.toml` and restored on launch

## Supported Formats

PNG, JPEG, GIF, BMP, WebP, TIFF, ICO, AVIF, plus zip, cbz, tar, tar.gz, tgz, 7z, cb7, rar, and cbr archives containing them. RAR support builds vendored unrar C++ sources, disable with `--no-default-features` if needed.

## Keyboard & Mouse

| Input | Action |
|---|---|
| `→` / `D`, `←` / `A` | Next / previous image (hold to scroll) |
| Mouse back/forward buttons | Previous / next image |
| Scroll wheel | Zoom toward cursor |
| Left-drag | Pan (when zoomed in) |
| Double-click | Reset zoom |
| Right-click | Context menu |
| `Esc` | Dismiss menus |

## Build & Run

Requires Rust edition 2024.

```
cargo build          # compile
cargo run            # run the viewer
cargo run -- <path>  # open a file or folder
cargo test           # run tests
cargo clippy         # lint
cargo fmt            # format
```

## Configuration

Settings persist to `config.toml` in the platform config directory
(`%APPDATA%\scryglass` on Windows, `~/.config/scryglass` on Linux,
`~/Library/Application Support/scryglass` on macOS). Unknown keys are
ignored, so configs survive upgrades in both directions.

## Architecture

scryglass follows the Elm Architecture (iced pattern):

- **`App`**: single source of truth for all state
- **`Message`**: enum driving all state transitions
- **`update()`**: handles messages, fires async tasks
- **`view()`**: pure rendering, no side effects

Images are loaded through iced's `image::allocate()` API, which returns GPU-resident `Allocation` objects. Holding an `Allocation` guarantees the texture renders immediately: no decode delay, no flicker. All I/O (directory scans, metadata, config writes) runs off the UI thread.

## License

MIT
