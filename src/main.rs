mod app;
mod cache;
mod config;
mod gif;
mod nav;
mod viewer;

fn main() -> anyhow::Result<()> {
    iced::application(app::boot, app::update, app::view)
        .title(app::title)
        .subscription(app::subscription)
        .run()?;
    Ok(())
}
