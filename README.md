# scryglass

A lightweight, blazing-fast image viewer built with [iced](https://github.com/iced-rs/iced) 0.14.

## Features

- **Instant navigation**: arrow keys, A/D keys, or mouse back/forward buttons flip through images, hold to scroll continuously
- **Flicker-free display**: images are pre-allocated as GPU textures via `image::allocate()` before being shown
- **Pre-fetching**: neighboring images (±5 by default) are decoded and uploaded to GPU memory in the background
- **Zoom modes**: Auto, Lock Zoom Ratio, Scale to Width/Height/Fit/Fill, plus scroll-wheel zoom toward the cursor and drag-to-pan
- **Animated GIF support**: frames are decoded with proper disposal-method compositing and animated at their native frame rate
- **"Open with…" support**: pass a file or folder as a CLI argument, and folders open at their first image
- **Drag-and-drop**: drop any image (or folder) to open it. The parent directory loads into a circular, wrap-around list
- **Natural sorting**: `img2` comes before `img10`, like a file manager
- **Filmstrip, slider, footer**: thumbnail strip, position slider, and an info footer (dimensions, file size, zoom, position), each individually toggleable
- **Context menu**: copy image / path / filename, reveal in file manager, file properties
- **Dark & light themes**: near-black chrome designed for photo viewing (default), light theme optional
- **Persistent settings**: zoom mode, layout, and theme are saved to `config.toml` and restored on launch

## Supported Formats

PNG, JPEG, GIF, BMP, WebP, TIFF, ICO, AVIF

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
