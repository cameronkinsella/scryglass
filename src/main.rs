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
        .font(iced_fonts::BOOTSTRAP_FONT_BYTES)
        .settings(iced::Settings {
            vsync: false,
            ..Default::default()
        })
        .run()?;
    Ok(())
}
