//! Settings dialog: the persisted knobs that don't have a menu home.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Element, Length};

use crate::app::SettingsMessage;
use crate::config::AppConfig;
use crate::ui::theme;

/// Render the settings card. `disk_cache_size` is `None` while the
/// thumbnail-store probe is still running.
pub fn settings<'a>(
    config: &AppConfig,
    disk_cache_size: Option<u64>,
    associations_registered: bool,
) -> Element<'a, SettingsMessage> {
    let switch = |label: &str, on: bool, msg: fn(bool) -> SettingsMessage| {
        toggler(on)
            .label(label.to_string())
            .on_toggle(msg)
            .size(15)
            .text_size(13)
            .width(Length::Fill)
    };

    let stepper =
        |label: &str, value: String, dec: Option<SettingsMessage>, inc: Option<SettingsMessage>| {
            let small = |t: &str, msg: Option<SettingsMessage>| {
                button(text(t.to_string()).size(13))
                    .on_press_maybe(msg)
                    .padding([1, 8])
                    .style(button::secondary)
            };
            row![
                text(label.to_string()).size(13).width(Length::Fill),
                small("−", dec),
                text(value).size(13).width(Length::Fixed(64.0)).center(),
                small("+", inc),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center)
        };

    let depth = config.prefetch_depth;
    let budget = config.cache_budget_mb;

    let mut rows = column![
        row![
            text("Settings").size(16),
            button(container(crate::ui::icons::question_circle().size(16)).center(Length::Fill),)
                .on_press(SettingsMessage::ShowHelp)
                .width(Length::Fixed(26.0))
                .height(Length::Fixed(26.0))
                .padding(0)
                .style(theme::icon_button),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        switch(
            "Read-only mode (no delete or rename)",
            config.read_only,
            |_| SettingsMessage::ToggleReadOnly
        ),
        switch("Confirm before deleting", config.confirm_delete, |_| {
            SettingsMessage::ToggleConfirmDelete
        }),
        rule::horizontal(1),
        stepper(
            "Prefetch depth",
            depth.to_string(),
            (depth > 1).then(|| SettingsMessage::SetPrefetchDepth(depth - 1)),
            (depth < 10).then(|| SettingsMessage::SetPrefetchDepth(depth + 1)),
        ),
        stepper(
            "Image cache budget",
            format!("{budget} MB"),
            (budget > 128).then(|| SettingsMessage::SetCacheBudget(budget - 128)),
            (budget < 4096).then(|| SettingsMessage::SetCacheBudget(budget + 128)),
        ),
        rule::horizontal(1),
        switch("Persistent thumbnails", config.disk_thumbs, |_| {
            SettingsMessage::ToggleDiskThumbs
        }),
    ]
    .spacing(10)
    .padding(18)
    .width(Length::Fixed(360.0));

    let size_label = disk_cache_size
        .map(crate::ui::format_file_size)
        .unwrap_or_else(|| "…".to_string());
    rows = rows.push(
        row![
            text(format!("Thumbnail store: {size_label}"))
                .size(13)
                .width(Length::Fill),
            button(text("Clear").size(13))
                .on_press(SettingsMessage::ClearDiskThumbs)
                .padding([3, 12])
                .style(button::secondary),
        ]
        .align_y(iced::Alignment::Center),
    );

    rows = rows.push(rule::horizontal(1));
    rows = rows.push(switch(
        "Hardware video decode",
        config.hardware_decode,
        |_| SettingsMessage::ToggleHardwareDecode,
    ));

    // Windows "Open with" needs a one-time per-user registration.
    if cfg!(target_os = "windows") {
        let (caption, action) = if associations_registered {
            ("scryglass is in the Open with menu for images", "Remove")
        } else {
            ("Offer scryglass when opening images", "Set up")
        };
        rows = rows.push(rule::horizontal(1));
        rows = rows.push(
            row![
                text(caption).size(13).width(Length::Fill),
                button(text(action).size(13))
                    .on_press(SettingsMessage::ToggleFileAssociations)
                    .padding([3, 12])
                    .style(button::secondary),
            ]
            .align_y(iced::Alignment::Center),
        );
    }

    crate::ui::overlay_card(container(rows).style(theme::panel), SettingsMessage::Close)
}

#[cfg(test)]
mod tests {
    use super::settings;
    use crate::config::AppConfig;
    use iced_test::simulator;

    #[test]
    fn renders_title_and_steppers() {
        let mut ui = simulator(settings(&AppConfig::default(), None, false));
        assert!(ui.find("Settings").is_ok());
        assert!(ui.find("Prefetch depth").is_ok());
        assert!(ui.find("Image cache budget").is_ok());
    }

    #[test]
    fn shows_the_thumbnail_store_size() {
        let mut ui = simulator(settings(&AppConfig::default(), Some(2048), false));
        assert!(ui.find("Thumbnail store: 2.0 KB").is_ok());
    }
}
