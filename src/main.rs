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

    // If another instance is running, hand it the path and exit; it opens the
    // file as a new window.
    if let ipc::Role::Forwarded = ipc::establish(initial_path.as_deref()) {
        return Ok(());
    }

    let boot = move || app::boot(initial_path.clone());

    // A daemon owns its windows: boot opens the first, and the app keeps
    // running until the last one closes (see update's Closed handler).
    iced::daemon(boot, app::update, app::view)
        .title(app::title)
        .theme(app::theme)
        .subscription(app::subscription)
        // .settings() replaces the whole struct, so it must precede .font()
        // (fonts accumulate inside settings).
        .settings(iced::Settings {
            vsync: false,
            ..Default::default()
        })
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .run()?;
    Ok(())
}
