mod anim;
mod app;
mod cache;
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

fn main() -> anyhow::Result<()> {
    // Restore the last window size. The close handler persists it.
    let initial = config::AppConfig::load();

    iced::application(app::boot, app::update, app::view)
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
            // Close requests route through update() so the config (window
            // size included) is saved before exit.
            exit_on_close_request: false,
            ..Default::default()
        })
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .run()?;
    Ok(())
}
