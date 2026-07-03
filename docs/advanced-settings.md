# Settings

scryglass keeps its settings in `config.toml`. Open it from **Settings → Open
Advanced Settings**, or find it next to the app in portable mode, otherwise under
your OS config directory. The file has three top-level sections:

- `[standard]`: every key here also has a control in the app.
- `[advanced]`: only editable in this file.
- `[managed]`: written by the app.

Saved edits apply live across all open windows, with no relaunch (decay-pipeline
changes take effect on the next image). The one exception is `[advanced.startup]`,
which needs a full restart. Unknown keys are ignored and missing keys fall back to
their default, so the file is safe to evolve and hand-edit. A syntax error is
reported and the previous settings are kept.

Durations are written as readable strings: `"15s"`, `"200ms"`, `"1m30s"`, or
`"never"` to disable.

## Standard settings (`[standard]`)

Every key in this section has an in-app control. Editing the file reaches the
same values.

### Browsing and cache (`[standard.browsing]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `prefetch_depth` | integer | `5` | How many images to decode ahead/behind the current one in each direction, so navigation is instant. `0` turns prefetch off. |
| `disk_thumbs` | bool | `true` | Persist thumbnails to disk so the filmstrip is instant on re-open. |

### Display (`[standard.display]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `theme` | `"Dark"` \| `"Light"` | `"Dark"` | UI color theme. |
| `zoom_mode` | enum | `"Auto"` | How zoom is chosen on open/navigate (`Auto`, `LockZoomRatio`, `ScaleToWidth`, `ScaleToHeight`, `ScaleToFit`, `ScaleToFill`). |
| `sort_key` | `"Name"` \| `"DateModified"` \| `"Size"` | `"Name"` | File ordering. |
| `sort_desc` | bool | `false` | Reverse the sort order. |
| `nearest_neighbor_zoom` | bool | `false` | Nearest-neighbor sampling above 100% zoom, so pixel art stays crisp. |
| `checkerboard` | bool | `false` | Draw a checkerboard behind images to reveal transparency. |

### File operations (`[standard.files]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `read_only` | bool | `false` | Hide and block all file modification (delete, rename). |
| `confirm_delete` | bool | `true` | Ask before moving a file to the recycle bin. |
| `mouse_nav` | bool | `true` | Use the mouse back/forward buttons to navigate. |

### Video (`[standard.video]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `volume` | float `0.0`-`1.0` | `1.0` | Playback volume. |
| `muted` | bool | `false` | Start muted. |
| `loop` | bool | `false` | Loop playback. |
| `hardware_decode` | bool | `true` | Use the GPU video decoder when available, falling back to software. |

### Chrome visibility (`[standard.chrome]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `toolbar` | bool | `true` | Top toolbar. |
| `filmstrip` | bool | `true` | Thumbnail filmstrip. |
| `slider` | bool | `true` | Position slider. |
| `footer` | bool | `true` | Footer status bar. |
| `info` | bool | `false` | EXIF info panel. |

## Advanced settings (`[advanced]`)

The keys in this section are only editable in this file.

### Scaling (`[advanced.scaling]`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `downscale_kernel` | enum | `"mitchell"` | Kernel used to shrink stills and animations to fit (`bilinear`, `mitchell`, `catmull-rom`, `lanczos3`). All fix the aliasing a plain bilinear tap leaves when shrinking past ~2x. Sharper kernels (`catmull-rom`, `lanczos3`) resolve more detail but can ring on text and hard edges. |
| `video_high_quality_scaling` | bool | `true` | Downscale a minified (shrunk-to-fit) video with the factor-aware kernel, matching still-image quality. Turn off to cut the per-frame GPU cost. Video at or above its native size is unaffected. |

### Resource model (`[advanced.resource]`)

scryglass reclaims a window's GPU and RAM as it moves to the background, and gives
it back when you return. A backgrounded window runs a forward-only **decay
pipeline**:

```
full-res VRAM  →demote→  view-res VRAM  →drop→  no VRAM  →evict→  no RAM
```

"Drop" frees the VRAM only (re-uploaded from the RAM copy on return). "Evict"
frees the RAM copy too (re-decoded from disk on return). Each step has its own
timer (a duration, or `"never"` to skip it), measured from when the window
entered the state. The steps can never run out of order, and re-focusing (or
scroll-zooming) a window restores it. The **unfocused** and **minimized** states
use the same controls, with different defaults.

| Key | Type | Default | Effect |
|---|---|---|---|
| `prefetch_vram` | `"full-res"` \| `"view-res"` \| `"none"` | `"view-res"` | What resolution a focused window's prefetched neighbors keep in VRAM. `full-res` is instant-crisp on navigation but heavy; `none` keeps them in RAM only. |
| `prefetch_scaler` | `"gpu"` \| `"cpu"` | `"gpu"` | Where a prefetched neighbor's view-res copy is produced. Both give identical pixels. `gpu` renders it through the display shader, briefly holding the neighbor's full decode in VRAM (up to ~268 MB per neighbor, two at once). `cpu` resamples at background priority with no extra VRAM but seconds of CPU per neighbor. |
| `prefetch_parallelism` | `"auto"` or a count | `"auto"` | How many prefetch neighbors decode at once, nearest first. Bounds the CPU burst and the peak RAM held by in-flight decodes (each holds its full decoded pixels until cached). `auto` is half the logical cores, at least 2; lower it on memory-tight machines, raise it to warm deep navigation faster. |
| `large_image_ram_budget` | percent or size | `"50%"` | Ceiling for a single image's decoded pixels in RAM. An image whose decode would exceed it opens downscaled to fit instead of failing (a 1-gigapixel image decodes to 4 GB). Accepts a share of the machine's RAM (`"50%"`) or an absolute size (`"2GB"`, `"500MB"`). Units `B`/`KB`/`MB`/`GB`/`TB` are powers of 1000, the `KiB` family powers of 1024. The budget only binds past the texture limit, so images within 8192 px per side (at most ~268 MB decoded) are never downscaled by it. |

#### `[advanced.resource.{unfocused,minimized}.{still,animated,video}]`

Each backgrounded state decays stills, animations, and video independently through a
`still`, an `animated`, and a `video` sub-table. The `Applies to` column lists which
of those sub-tables take each key.

| Key | Applies to | Type | Default (unfocused / minimized) | Effect |
|---|---|---|---|---|
| `demote_vram_after` | still | duration \| `"never"` | `"15s"` / `"never"` | Demote the on-screen image from full-res to view-res VRAM. |
| `drop_vram_after` | still | duration \| `"never"` | `"never"` / `"0s"` | Drop the on-screen image's VRAM entirely (it falls back to its thumbnail until you return). |
| `evict_ram` | still, animated | `"never"` \| duration \| `"dynamic"` | still `"never"` / `"dynamic"`, animated `"never"` / `"30s"` | When to evict the full-resolution copy (an animation's frames) from RAM, re-decoded from disk on return. A duration is a fixed delay. `dynamic` scales the delay by decode time, between `evict_ram_min` and `evict_ram_max`. `never` always keeps it. |
| `evict_ram_min` | still, animated | duration | `"30s"` / `"1m"` | `dynamic` only: the evict delay for an image that decodes instantly. |
| `evict_ram_max` | still, animated | duration | `"10m"` / `"5m"` | `dynamic` only: the evict delay for an image at the latency ceiling. |
| `max_decode_latency` | still, animated | duration | `"200ms"` / `"200ms"` | `dynamic` only: an image slower than this to decode (read + decode) is never evicted, so slow storage stays resident. |
| `evict_session_after` | video | duration \| `"never"` | `"never"` / `"5s"` | Release an open video's whole decode session (decode threads, hardware decoder, audio sink, GPU textures) this long after the window is backgrounded. The last frame stays frozen and the video re-opens at the saved position on return. `never` keeps the session alive. |
| `pause` | video (minimized only) | bool | `true` | Pause an open video while the window is minimized. |

#### `[advanced.resource.{unfocused,minimized}.prefetch]`

A backgrounded window sheds its prefetched neighbors separately from the
on-screen image, and this sub-table controls when and how fast. Shedding walks
inward ring by ring from the furthest neighbors: each step releases the most
distant remaining ring (up to one image per side, both sides together), so the
neighbors you are most likely to see next are the last to go. A released
neighbor frees both its VRAM and its RAM; returning to the window re-warms the
look-ahead (re-decoding what was shed).

| Key | Type | Default (unfocused / minimized) | Effect |
|---|---|---|---|
| `drop_on` | `"immediately"` \| `"demote"` \| `"drop"` \| `"evict"` | `"demote"` / `"immediately"` | The event the shedding counts from. `immediately` counts from entering the state. The others count from the on-screen image's decay reaching that point of its pipeline. An anchor stage the pipeline skips falls through to the next stage that actually runs (`"demote"` still sheds with the drop stage when demote is `"never"`, and with an animation's evict or a video's session release). Only when nothing at or after the anchor runs is the prefetch kept. |
| `drop_after` | duration | `"0s"` / `"15s"` | How long after the event the first ring is released. |
| `drop_interval` | duration | `"5s"` / `"5s"` | The pause between one ring and the next. `0s` sheds everything in one sweep. |

#### `[advanced.resource.working_set]` (Windows only)

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
| `trim_when` | `"never"` \| `"all-unfocused"` \| `"all-minimized"` | `"never"` | The background condition that arms the trim. `all-unfocused` fires whenever scryglass is not the foreground app (any window still visible). `all-minimized` fires only once every window is minimized (fully hidden), hiding the re-fault behind the restore you are already waiting on. |
| `trim_after` | duration | `"10s"` | How long the condition must hold, uninterrupted, before the trim fires. A refocus or restore within this grace period cancels it. |

### Startup (`[advanced.startup]`)

Unlike everything above, these are read once when scryglass starts and are fixed
for the life of the process. Changing one takes effect after a full restart: close
every scryglass window, then launch again. Opening a file while scryglass is
already running joins the existing process, which keeps its startup settings.

| Key | Type | Default | Effect |
|---|---|---|---|
| `present_mode` | enum | `"mailbox"` on Windows, `"auto"` elsewhere | How rendered frames are handed to the display (values below). |

| Value | Meaning |
|---|---|
| `"auto"` | iced's default: vsync through the first synced mode the driver offers. |
| `"mailbox"` | Synced to the display refresh without blocking: tear-free playback, and live window resizes stay clean where the blocking modes flicker at the window edge on Windows ([wgpu#5374](https://github.com/gfx-rs/wgpu/issues/5374)). |
| `"fifo"` | The classic vsync queue. The only mode every driver must support ([Vulkan spec, VkPresentModeKHR](https://docs.vulkan.org/refpages/latest/refpages/source/VkPresentModeKHR.html)). |
| `"fifo-relaxed"` | Vsync that tears instead of stalling when a frame misses the vertical blank. |
| `"immediate"` | No sync at all: the lowest latency, may tear anywhere. |
| `"no-vsync"` | Vsync off with graceful fallback (`immediate` where offered, else `mailbox`, else `fifo`), so it works on any driver. |

Not every driver offers every mode. AMD's Windows Vulkan driver has no
mailbox. Requesting a missing mode would fail at launch, so on Windows scryglass
probes the driver first and quietly falls back to `"auto"` when the requested mode
is absent. `"auto"`, `"fifo"`, and `"no-vsync"` can never be missing. On other
platforms the value is passed through as-is. The probe's answer is cached
(`present-probe.toml`, stored like the thumbnail cache) and refreshed whenever the
GPU or its driver changes, so it costs one launch per driver, not every launch.

The `ICED_PRESENT_MODE` environment variable (values `vsync`, `no_vsync`,
`immediate`, `fifo`, `fifo_relaxed`, `mailbox`) overrides this setting without
validation, which is handy for a quick experiment.

## Managed state (`[managed]`)

The app writes this section for itself. Editing it by hand works but is rarely
needed.

### Window (`[managed.window]`)

The last window's geometry, so a new window reopens where the last one closed.

| Key | Type | Effect |
|---|---|---|
| `width`, `height` | float | Restored windowed size. |
| `x`, `y` | float (optional) | Restored windowed position. |
| `maximized` | bool | Reopen maximized. |
| `fullscreen` | bool | Reopen fullscreen. |
