// Release builds are a GUI app, no console window. Debug builds keep
// the console for decoder and panic output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod app;
mod components;
mod config;
#[cfg(feature = "video")]
mod gpu_keepalive;
mod ipc;
mod media;
mod nav;
mod platform;
mod ui;
#[cfg(feature = "update-check")]
mod update_check;
#[cfg(feature = "video")]
mod video;
#[cfg(not(feature = "video"))]
#[path = "video_stub.rs"]
mod video;

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // A file passed by the OS (file association, "Open with", or the shell).
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);

    // Off Windows there is no driver probe, so the present mode is applied
    // up front, before any thread exists, keeping the env write sound.
    #[cfg(not(target_os = "windows"))]
    apply_present_mode();

    // If another instance is running, hand it the path and exit. It opens the
    // file as a new window.
    if let ipc::Role::Forwarded = ipc::establish(initial_path.as_deref()) {
        return Ok(());
    }

    // On Windows the mode may need a GPU driver probe, so it runs only in
    // the process that will own windows. A forwarding launch stays snappy.
    #[cfg(target_os = "windows")]
    apply_present_mode();

    let boot = move || app::boot(initial_path.clone());

    // A daemon owns its windows: boot opens the first, and the app keeps
    // running until the last one closes (see update's Closed handler).
    iced::daemon(boot, app::update, app::view)
        .title(app::title)
        .theme(app::theme)
        .subscription(app::subscription)
        // .settings() replaces the whole struct, so it must precede .font()
        // (fonts accumulate inside settings). vsync picks the synced auto mode
        // when `[advanced.startup]` present_mode is "auto". Any other value
        // has already been handed to iced by apply_present_mode, which
        // overrides this flag.
        .settings(iced::Settings {
            vsync: true,
            ..Default::default()
        })
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .run()?;
    Ok(())
}

/// Hand iced the configured `[advanced.startup]` present mode through
/// `ICED_PRESENT_MODE`, the only channel iced exposes for the explicit wgpu
/// modes. A value already set in the environment wins, unvalidated, exactly
/// as iced would read it. The config is re-read with error reporting in
/// `boot`. This early read only feeds the env var.
fn apply_present_mode() {
    if std::env::var_os("ICED_PRESENT_MODE").is_some() {
        return;
    }
    let mode = config::AppConfig::load_reporting()
        .0
        .advanced
        .startup
        .present_mode;
    // iced treats configuring a mode the driver lacks as fatal, so a mode
    // that can be missing is checked against the driver first. Only Windows
    // has probe glue. Other platforms pass the configured mode through.
    #[cfg(target_os = "windows")]
    let mode = if mode.needs_probe() {
        mode.resolve(platform::supported_present_modes().as_deref())
    } else {
        mode
    };
    let Some(value) = mode.env_value() else {
        return;
    };
    // SAFETY: on Windows environment access goes through OS calls that the
    // kernel synchronizes. Elsewhere this runs before any thread is spawned.
    unsafe { std::env::set_var("ICED_PRESENT_MODE", value) };
}
