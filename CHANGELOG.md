# Changelog

Notable changes to scryglass, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.3.0

### Added

- One process for all windows: opening another file joins the running
  instance as a new window instead of launching a second copy.
- Huge images open through a tiled level-of-detail pyramid within a
  configurable RAM budget, so gigapixel files pan and zoom smoothly
  instead of failing or stalling on a single giant texture.
- A configurable resource model that returns RAM and VRAM from background
  windows: per-media (still, animation, video) demotion, eviction, and
  prefetch shedding for unfocused and minimized windows, plus an optional
  Windows working-set trim. Fully documented in docs/advanced-settings.md.
- Factor-aware, linear-light downscaling for stills and animations with a
  choice of kernel (`downscale_kernel`), and matching high-quality video
  downscaling (`video_high_quality_scaling`).
- A startup present-mode setting backed by a cached driver capability
  probe (`startup.present_mode`).
- Portable mode: a `data/` folder beside the executable keeps config and
  thumbnails with the install.
- config.toml edits reload live across all open windows, and Settings
  gains an Open Advanced Settings button. A malformed config is reported
  and set aside instead of silently replaced, and saves are atomic.
- Videos inside archives show a first-frame thumbnail.
- Prefetch tuning: parallelism, VRAM residency mode, nearest-first order,
  background-priority decodes, and a depth of `0` to turn it off.

### Changed

- Stills render through a GPU shader surface, pixel-exact at the render
  size, replacing iced's built-in image widget.
- Video frames are paced by the playback clock on a steady 120Hz tick,
  so playback stays smooth in every window at once.
- Animations share their decoded frames and decay state across windows,
  and stills share one texture across windows.
- Full window geometry (size, position, maximized, fullscreen) persists
  and replays on reopen. Snap layouts are not saved as the window size.
- The crisp-pixels toggle is now "Nearest-neighbor zoom", and its config
  key is `nearest_neighbor_zoom`.
- config.toml is grouped into three tiers: `[standard]` for settings with
  in-app controls, `[advanced]` for file-only settings (scaling, the
  resource model, startup), and the app-managed `[managed]`. Existing
  config files reset to defaults on first launch.
- A new original app icon: a magnifying glass over a stack of pancakes.
- Faster navigation away from a playing video.

### Fixed

- Video geometry tracks a live resize exactly instead of lagging a frame
  behind at a slight offset.
- A video restoring from the background shows its thumbnail instead of a
  blank frame until playback reopens.
- A video stream with backwards or missing timestamps no longer freezes
  playback.
- A crash when files disappeared from the folder while a navigation onto
  them was still pending.
- A folder shrinking behind a far-scrolled filmstrip no longer crashes.
- A corrupt or malicious image with an oversized header no longer aborts
  the whole app trying to allocate gigabytes.
- A video that cannot be opened now reports the error instead of spinning
  forever, and no longer respawns a decode loop when set to loop.
- The thumbnail cache stays within its memory budget while a large folder
  fills in the background.
- A folder that loses the on-screen file (deleted outside the app) keeps
  the view and cursor in step instead of desyncing.
- Fullscreen wheel zoom anchors under the cursor instead of a toolbar
  height above it.
- Quitting from the menu saves the window geometry the way the close
  button does.
- Two windows viewing the same very large image no longer fight over its
  tiles, and one window's navigation no longer cancels another's decodes.

## 0.2.1

### Fixed

- The window reopens at its last windowed size, even when closed while
  maximized or fullscreen.

## 0.2.0

### Added

- Optional mouse navigation: hover the left or right edge of the image
  for an arrow, then click anywhere in that strip to step, or hold to
  keep going. Toggle it in Settings.
- Settings shows the current version and has a "Check for updates" button
  that compares it against the latest GitHub release and links to a newer
  one when there is one.
- AVIF still images and AV1 video, decoded through FFmpeg with dav1d.
- Hardware-accelerated video decode with a software fallback, and video
  rendered through a GPU YUV shader.
- Frame-by-frame stepping and a sticky loop toggle in the video player.
  Looping restarts seamlessly, with no pause at the loop point.
- A precise zoom slider in the footer.
- Windows default-app registration: turn on file associations in
  Settings, then pick scryglass under Settings > Apps > Default apps. No
  admin needed.
- A Windows installer, a Linux AppImage, and an unsigned macOS `.dmg`
  among the release downloads, alongside a slim `.tar.gz` (Linux, macOS)
  or `.zip` (Windows) for `cargo binstall` and portable use.

### Changed

- Video controls fade in and out, hide with an idle cursor, and reappear
  on volume and seek keys.
- Help and settings open as scrollable panels that dismiss on an outside
  click, with section headers and a help button in settings.
- The application window now has a minimum size.
- Releases are tagged automatically from the crate version, one tag per
  version-bump commit, and a release fails if its tag and the
  `Cargo.toml` version disagree.
- The open folder refreshes automatically when other programs add or
  remove files, so the filmstrip matches what is on disk.
- The rename box selects just the name and warns when the typed
  extension would misrepresent the file's contents.
- Thumbnails generate outward from the cursor and reprioritize when you
  jump, so previews nearest where you are now fill in first instead of
  finishing the spot you left.
- Scrolling the filmstrip away from the current image loads only the
  thumbnails on screen, filling from the middle out, so a quick scrub past
  hundreds of files no longer queues them all on slow storage.

### Fixed

- Opening an unsupported file type now says so plainly, instead of a
  confusing "start file not found in directory listing".
- Turning the filmstrip on mid-session opens it on the current image,
  instead of scrolled to the start of the directory.
- Video thumbnails load through the same throttled queue as images and
  cancel when you navigate away, instead of grabbing first frames all at
  once and ignoring where you have moved to.
- The checkerboard backdrop repaints when the theme changes.
- The drag-and-drop prompt stays centered when it wraps onto two lines.
- The thumbnail store size is shown even when persistent thumbnails are
  off.
- Typing in the rename box no longer triggers viewer and video
  shortcuts.
- Renaming the video you are watching no longer fails because the file
  is in use.
- Renaming a file into or out of a video format now shows or hides the
  player and its controls right away, with no navigating away and back.
- A file that can't be decoded shows an error in the image area and no
  longer blocks the cursor; navigation moves right past it.
- Toolbar dropdowns stay open when you choose a zoom mode or sort key, or
  click the panel itself; only a click outside dismisses them.
- The right-click context menu stays fully on screen near a window edge,
  flipping its position instead of spilling off.
- Scrubbing with the position slider or a held arrow key moves straight
  onto every frame, with a spinner for frames not loaded yet, and loads
  the one you settle on in place. The old preview bubble is gone.
- The filmstrip follows the cursor: centered while you drag the slider,
  scrolled just enough to stay on screen for arrow keys and clicks, and a
  thumbnail you click opens instantly with a spinner if it isn't loaded.

## 0.1.0

Initial release. Smooth navigation built for slow storage, archives
browsable as folders, GIF/APNG/WebP animation, a full
video player on statically linked FFmpeg, HEIC, JPEG XL, SVG, camera RAW
previews, persistent disk thumbnails with privacy hygiene, recycle-bin
delete and rename, native file manager sorting, and dark and light
themes.
