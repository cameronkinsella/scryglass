# Architecture

Directory map. Each module carries its own rationale in a `//!` doc, so this
file only says what lives where.

    src/
      main.rs           process entry: single-instance handshake, then iced
      ipc.rs            single-instance socket, forwards opens to the primary
      nav.rs            directory listing, sort, wrap-around cursor
      anim.rs           per-window animation playback over shared frames
      config/           persisted settings: ui, resource decay tree, startup
      platform/         OS glue: shell integration, present-mode probe
      app/              iced daemon core: state, messages, boot, subscriptions
        update/         message handlers: navigation, media, windows, decay
          media_tasks/  async task spawners: loads, tiles, thumbs, metadata
      components/       widgets, one directory each: mod.rs state + view/widget
      media/            decode, caches, and the shared texture store
        decoders/       one file per format family behind the ImageFormat trait
        store/          typestate texture lifecycle, shared across windows
      ui/               rendering: theme, geometry math, the two GPU surfaces
        image_surface/  still and animation surface: resident textures, tiles,
                        the upload thread, and the committed image.spv
        video_surface/  video surface: YUV planes and the committed yuv.spv
      video/            playback sessions: demux, decode threads, audio clock
      video_stub.rs     no-op stand-ins when the video feature is off
    shaders/            rust-gpu crates compiled to the committed .spv files
      common/           pure-math kernel weights and sRGB transfer, host-tested
      image/            RGBA display and bake shader
      yuv/              YUV video shader with the high-quality downscale
    xtask/              packaging and shader-build helper commands

Conventions: app owns state and update logic, components own widgets, media
owns bytes-to-pixels, ui owns pixels-to-screen. A new format goes under
media/decoders, a new widget under components, new OS glue under platform.

Update handlers split by message source. A component's update handles only
interactions with its own widget and delegates the real work. app/update
handles everything that is not a widget interaction: OS window events
(window.rs), async completions (media.rs), the open flow (open.rs), and the
shared domain machinery both layers call into (navigation, media_tasks,
decay). Cross-cutting UI policy (menu auto-dismiss, modal keyboard capture)
and window routing live in app/update/mod.rs. Nothing in app/update calls
into a component's update.
