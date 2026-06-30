# Advanced settings

scryglass keeps its settings in `config.toml` (open it from **Settings → Open
Advanced Settings**, or find it next to the app in portable mode, otherwise under
your OS config directory). Most settings have an in-app control; the ones here can
only be changed by editing the file. Saved edits apply live across all open windows,
with no relaunch (decay-pipeline changes take effect on the next image); the one
exception is the `[startup]` section, which needs a full restart. Every key
has a sensible default, so you only set what you want to change. Unknown keys are
ignored and missing keys fall back to their default, so the file is safe to evolve
and hand-edit; a syntax error is reported and the previous settings are kept.

Durations are written as readable strings: `"15s"`, `"200ms"`, `"1m30s"`, or
`"never"` to disable.

## Browsing and cache

| Key | Type | Default | Effect |
|---|---|---|---|
| `prefetch_depth` | integer | `5` | How many images to decode ahead/behind the current one in each direction, so navigation is instant. |
| `disk_thumbs` | bool | `true` | Persist thumbnails to disk so the filmstrip is instant on re-open. |

## Display

| Key | Type | Default | Effect |
|---|---|---|---|
| `theme` | `"Dark"` \| `"Light"` | `"Dark"` | UI color theme. |
| `zoom_mode` | enum | `"Auto"` | How zoom is chosen on open/navigate (`Auto`, `LockZoomRatio`, `ScaleToWidth`, `ScaleToHeight`, `ScaleToFit`, `ScaleToFill`). |
| `sort_key` | `"Name"` \| `"DateModified"` \| `"Size"` | `"Name"` | File ordering. |
| `sort_desc` | bool | `false` | Reverse the sort order. |
| `crisp_pixels` | bool | `false` | Nearest-neighbor sampling above 100% zoom (crisp pixel art). |
| `downscale_kernel` | enum | `"mitchell"` | Kernel used to shrink stills and animations to fit (`bilinear`, `mitchell`, `catmull-rom`, `lanczos3`). All fix the aliasing a plain bilinear tap leaves when shrinking past ~2x; sharper kernels (`catmull-rom`, `lanczos3`) resolve more detail but can ring on text and hard edges. |
| `show_checkerboard` | bool | `false` | Draw a checkerboard behind images to reveal transparency. |

## File operations

| Key | Type | Default | Effect |
|---|---|---|---|
| `read_only` | bool | `false` | Hide and block all file modification (delete, rename). |
| `confirm_delete` | bool | `true` | Ask before moving a file to the recycle bin. |
| `mouse_nav` | bool | `true` | Use the mouse back/forward buttons to navigate. |

## Video

| Key | Type | Default | Effect |
|---|---|---|---|
| `video_volume` | float `0.0`–`1.0` | `1.0` | Playback volume. |
| `video_muted` | bool | `false` | Start muted. |
| `video_loop` | bool | `false` | Loop playback. |
| `hardware_decode` | bool | `true` | Use the GPU video decoder when available, falling back to software. |
| `video_high_quality_scaling` | bool | `true` | Downscale a minified (shrunk-to-fit) video with the factor-aware kernel, matching still-image quality, instead of one bilinear tap. Turn off to cut the per-frame GPU cost. Video at or above its native size is unaffected. |

## Chrome visibility

| Key | Type | Default | Effect |
|---|---|---|---|
| `show_toolbar` | bool | `true` | Top toolbar. |
| `show_filmstrip` | bool | `true` | Thumbnail filmstrip. |
| `show_slider` | bool | `true` | Position slider. |
| `show_footer` | bool | `true` | Footer status bar. |
| `show_info` | bool | `false` | EXIF info panel. |

## Window state (managed automatically)

These persist the last window's geometry so a new window reopens where the last one
closed; you normally don't edit them by hand.

| Key | Type | Effect |
|---|---|---|
| `window_width`, `window_height` | float | Restored windowed size. |
| `window_x`, `window_y` | float (optional) | Restored windowed position. |
| `window_maximized` | bool | Reopen maximized. |
| `window_fullscreen` | bool | Reopen fullscreen. |

## Startup (`[startup]`)

Unlike everything above, these are read once when scryglass starts and are fixed
for the life of the process. Changing one takes effect after a full restart: close
every scryglass window, then launch again. Opening a file while scryglass is
already running joins the existing process, which keeps its startup settings.

| Key | Type | Default | Effect |
|---|---|---|---|
| `present_mode` | enum | `"mailbox"` on Windows, `"auto"` elsewhere | How rendered frames are handed to the display; values below. |

| Value | Meaning |
|---|---|
| `"auto"` | iced's default: vsync through the first synced mode the driver offers. |
| `"mailbox"` | Synced to the display refresh without blocking: tear-free playback, and live window resizes stay clean where the blocking modes flicker at the window edge on Windows ([wgpu#5374](https://github.com/gfx-rs/wgpu/issues/5374)). |
| `"fifo"` | The classic vsync queue. The only mode every driver must support ([Vulkan spec, VkPresentModeKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html)). |
| `"fifo-relaxed"` | Vsync that tears instead of stalling when a frame misses the vertical blank. |
| `"immediate"` | No sync at all: the lowest latency, may tear anywhere. |
| `"no-vsync"` | Vsync off with graceful fallback (`immediate` where offered, else `mailbox`, else `fifo`), so it works on any driver. |

Not every driver offers every mode; notably, AMD's Windows Vulkan driver has no
mailbox. Requesting a missing mode would fail at launch, so on Windows scryglass
probes the driver first and quietly falls back to `"auto"` when the requested mode
is absent. `"auto"`, `"fifo"`, and `"no-vsync"` can never be missing. On other
platforms the value is passed through as-is. The probe's answer is cached
(`present-probe.toml`, stored like the thumbnail cache) and refreshed whenever the
GPU or its driver changes, so it costs one launch per driver, not every launch.

The `ICED_PRESENT_MODE` environment variable (values `vsync`, `no_vsync`,
`immediate`, `fifo`, `fifo_relaxed`, `mailbox`) overrides this setting without
validation, which is handy for a quick experiment.

## Resource model (`[resource]`)

scryglass reclaims a window's GPU and RAM as it moves to the background, and gives
it back when you return. This is fully tunable. The defaults are a balance; a slim
build can reclaim more aggressively, and a "blazing fast always" build can disable
decay entirely.

A backgrounded window runs a forward-only **decay pipeline**:

```
full-res VRAM  →demote→  view-res VRAM  →drop→  no VRAM  →evict→  no RAM
```

"Drop" frees the VRAM only (re-uploaded from the RAM copy on return); "evict"
frees the RAM copy too (re-decoded from disk on return). Each step has its own
timer (a duration, or `"never"` to skip it), measured from when the window
entered the state. The steps can never run out of order, and re-focusing (or
scroll-zooming) a window restores it. The **unfocused** and **minimized** states
use the same controls, with different defaults.

| Key | Type | Default | Effect |
|---|---|---|---|
| `prefetch_vram` | `"full-res"` \| `"view-res"` \| `"none"` | `"view-res"` | What resolution a focused window's prefetched neighbors keep in VRAM. `full-res` is instant-crisp on navigation but heavy; `none` keeps them in RAM only. |
| `prefetch_scaler` | `"gpu"` \| `"cpu"` | `"gpu"` | Where a prefetched neighbor's view-res copy is produced. Both give identical pixels. `gpu` renders it through the display shader, briefly holding the neighbor's full decode in VRAM (up to ~268 MB per neighbor, two at once); `cpu` resamples at background priority with no extra VRAM but seconds of CPU per neighbor. |
| `prefetch_parallelism` | `"auto"` or a count | `"auto"` | How many prefetch neighbors decode at once, nearest first. Bounds the CPU burst and the peak RAM held by in-flight decodes (each holds its full decoded pixels until cached). `auto` is half the logical cores, at least 2; lower it on memory-tight machines, raise it to warm deep navigation faster. |
| `large_image_ram_budget` | percent or size | `"50%"` | Ceiling for a single image's decoded pixels in RAM. An image whose decode would exceed it opens downscaled to fit instead of failing (a 1-gigapixel image decodes to 4 GB). Accepts a share of the machine's RAM (`"50%"`) or an absolute size (`"2GB"`, `"500MB"`); units `B`/`KB`/`MB`/`GB`/`TB` are powers of 1000, the `KiB` family powers of 1024. The budget only binds past the texture limit, so images within 8192 px per side (at most ~268 MB decoded) are never downscaled by it. |

### `[resource.unfocused.{still,animated,video}]` and `[resource.minimized.{still,animated,video}]`

Each backgrounded state (**unfocused**, **minimized**) decays stills, animations, and
video independently, so it has a `still`, an `animated`, and a `video` sub-table. The
`still` table takes all the keys below. An **animation** has no governed VRAM tier, so
its `animated` table takes **only** the eviction keys (`evict_ram`, `evict_ram_min`,
`evict_ram_max`, `max_decode_latency`), not `demote_vram_after` or `drop_vram_after`. Re-decoding an animation is costly, so the `animated` defaults
keep frames in RAM longer than stills: `evict_ram = "never"` when unfocused,
`evict_ram = "30s"` when minimized. A **video** is a continuous stream with no tier at
all, so its `video` table takes a single timer for when to release the whole decode
session (see below).

| Key | Type | Default (still: unfocused / minimized) | Effect |
|---|---|---|---|
| `demote_vram_after` | duration \| `"never"` | `"15s"` / `"never"` | **(still only)** Demote the on-screen image from full-res to view-res VRAM (and drop the prefetch look-ahead). |
| `drop_vram_after` | duration \| `"never"` | `"never"` / `"0s"` | **(still only)** Drop the on-screen image's VRAM entirely (it falls back to its thumbnail until you return). |
| `evict_ram` | `"never"` \| duration \| `"dynamic"` | `"dynamic"` / `"dynamic"` | When to evict the full-resolution copy from RAM (re-decoded from disk on return). A duration is a fixed delay; `dynamic` decides per image from how long it took to decode (see below); `never` always keeps it. |
| `evict_ram_min` | duration | `"30s"` / `"15s"` | `dynamic` only: the evict delay for an image that decodes instantly. |
| `evict_ram_max` | duration | `"10m"` / `"5m"` | `dynamic` only: the evict delay for an image right at the latency ceiling. |
| `max_decode_latency` | duration | `"200ms"` / `"200ms"` | `dynamic` only: an image that took longer than this to decode (read + decode) is never evicted, so slow storage (a NAS) stays resident. |

The `video` sub-table has a single key:

| Key | Type | Default (unfocused / minimized) | Effect |
|---|---|---|---|
| `evict_session_after` | duration \| `"never"` | `"never"` / `"5s"` | When to release an open video's whole decode session (its decode threads, hardware decoder, audio sink, and GPU textures). The last frame stays frozen on screen and the video re-opens at the saved position when you return. `never` keeps the session alive (a minimized window still pauses it, see `pause_video`). |

`[resource.minimized]` additionally has, at the state level (not per media kind):

| Key | Type | Default | Effect |
|---|---|---|---|
| `pause_video` | bool | `true` | Pause an open video while the window is minimized. |

**Dynamic eviction** scales the RAM-evict delay linearly with the image's measured
decode time: a fast (SSD) image is reclaimed soon after `evict_ram_min`, a slower
one waits up to `evict_ram_max`, and anything past `max_decode_latency` is kept
indefinitely. Evicted images re-decode from disk when you return to the window
(showing the thumbnail meanwhile), so this trades a little restore latency for less
RAM on local storage while never punishing slow network drives.

**Video session release** frees the heaviest resource a video holds: its whole
decode pipeline (decode threads, the hardware decoder, the audio sink, and the GPU
plane textures). So memory scales with the videos you can actually see, not with
every window left open. A released video that is still visible (an unfocused window)
keeps its last frame frozen on screen, so it looks paused rather than blank; a
minimized window is hidden, so it drops the frame too and repaints when you restore
it. Either way it re-opens at the saved position and play state. Re-opening pays a
brief decoder warm-up (a demux seek plus the first frame), which is why a quick
alt-tab back within the grace period keeps the session. An archive video re-uses the file it was already extracted to, so it never
re-extracts. While a video is released its transport controls disappear and keyboard
playback keys do nothing until it returns, so unfocused (still visible) videos
default to never releasing; only minimized (hidden) ones do.

### `[resource.working_set]` (Windows only)

Decay frees a window's *own* GPU and RAM, but it cannot shrink the process's
irreducible baseline (the renderer, the GPU driver, the decoders). On Windows,
`EmptyWorkingSet` can: it hands the whole process's resident pages back to the OS,
so the idle footprint drops below what decay alone reaches. The pages fault back in
when next touched, so this is a footprint-vs-restore-latency trade, not a free win.
It is process-global, so it fires only once the *whole* app is in the background,
never per window. Off by default. This table is ignored on macOS and Linux, where
the OS reclaims idle memory on its own.

| Key | Type | Default | Effect |
|---|---|---|---|
| `trim_when` | `"never"` \| `"all-unfocused"` \| `"all-minimized"` | `"never"` | The background condition that arms the trim. `all-unfocused` fires whenever scryglass is not the foreground app (any window still visible); `all-minimized` only once every window is minimized (fully hidden), which hides the re-fault behind the restore you are already waiting on. |
| `trim_after` | duration | `"10s"` | How long the condition must hold, uninterrupted, before the trim fires. A refocus or restore within this grace period cancels it. |
