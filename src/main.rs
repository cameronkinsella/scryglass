mod app;
mod cache;
mod config;
mod gif;
mod media;
mod nav;
mod platform;
mod ui;

fn main() -> anyhow::Result<()> {
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
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .run()?;
    Ok(())
}
