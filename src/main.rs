// Release builds are a GUI app, no console window. Debug builds keep
// the console for decoder and panic output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod app;
mod cache;
mod components;
mod config;
mod media;
mod nav;
mod platform;
mod ui;
#[cfg(feature = "video")]
mod video;
#[cfg(not(feature = "video"))]
#[path = "video_stub.rs"]
mod video;

use std::path::PathBuf;

/// Decode the embedded window icon. The icon API that takes encoded
/// bytes sits behind iced's codec feature, which is off, so decode with
/// the image crate the pipeline already uses.
fn window_icon() -> Option<iced::window::Icon> {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    iced::window::icon::from_rgba(img.into_raw(), w, h).ok()
}

fn main() -> anyhow::Result<()> {
    // Restore the last window size. The close handler persists it.
    let initial = config::AppConfig::load();
    // A file passed by the OS (file association, "Open with", or the shell).
    let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
    let boot = move || app::boot(initial_path.clone());

    iced::application(boot, app::update, app::view)
        .title(app::title)
        .theme(app::theme)
        .subscription(app::subscription)
        // .settings() replaces the whole settings struct, so it must come
        // before .font(), because fonts accumulate inside settings.
        .settings(iced::Settings {
            vsync: false,
            ..Default::default()
        })
        .window(iced::window::Settings {
            size: iced::Size::new(initial.window_width, initial.window_height),
            icon: window_icon(),
            // Close requests route through update() so the config (window
            // size included) is saved before exit.
            exit_on_close_request: false,
            ..Default::default()
        })
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .run()?;
    Ok(())
}
