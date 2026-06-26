# Changelog

Notable changes to scryglass, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
