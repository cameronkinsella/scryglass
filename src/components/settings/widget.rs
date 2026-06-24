//! Settings dialog: the persisted knobs that don't have a menu home.

use iced::widget::{button, column, container, row, rule, text, toggler};
use iced::{Element, Length};

use crate::app::SettingsMessage;
use crate::config::AppConfig;
use crate::ui::theme;

/// Render the settings card.
#[cfg_attr(not(feature = "disk-thumbs"), allow(unused_variables))]
pub fn settings<'a>(
    config: &AppConfig,
    disk_cache_size: Option<u64>,
    associations_registered: bool,
    #[cfg(feature = "update-check")] update_status: Option<&crate::update_check::UpdateStatus>,
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
    ]
    .spacing(10)
    .padding(18)
    .width(Length::Fixed(360.0));

    #[cfg(feature = "disk-thumbs")]
    {
        rows = rows.push(rule::horizontal(1));
        rows = rows.push(switch("Persistent thumbnails", config.disk_thumbs, |_| {
            SettingsMessage::ToggleDiskThumbs
        }));
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
    }

    #[cfg(feature = "video")]
    {
        rows = rows.push(rule::horizontal(1));
        rows = rows.push(switch(
            "Hardware video decode",
            config.hardware_decode,
            |_| SettingsMessage::ToggleHardwareDecode,
        ));
    }

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

    rows = rows.push(rule::horizontal(1));
    rows = rows.push(
        text(concat!("scryglass v", env!("CARGO_PKG_VERSION")))
            .size(11)
            .style(theme::secondary_text),
    );
    #[cfg(feature = "update-check")]
    {
        rows = rows.push(update_section(update_status));
    }

    crate::ui::overlay_card(rows, SettingsMessage::Close)
}

/// A check button with the last result styled inline to its right.
#[cfg(feature = "update-check")]
fn update_section<'a>(
    status: Option<&crate::update_check::UpdateStatus>,
) -> Element<'a, SettingsMessage> {
    use crate::ui::icons;
    use crate::update_check::UpdateStatus;

    let checking = matches!(status, Some(UpdateStatus::Checking));
    let check = button(text("Check for updates").size(13))
        // Disabled mid-check so a double press can't fire two requests.
        .on_press_maybe((!checking).then_some(SettingsMessage::CheckForUpdates))
        .padding([3, 12])
        .style(button::secondary);

    let Some(status) = status else {
        return check.into();
    };
    let result: Element<'a, SettingsMessage> = match status {
        UpdateStatus::Checking => text("Checking…")
            .size(13)
            .style(theme::secondary_text)
            .into(),
        UpdateStatus::UpToDate => row![
            icons::check_lg().size(13).style(theme::success_text),
            text("Up to date").size(13).style(theme::success_text),
        ]
        .spacing(5)
        .align_y(iced::Alignment::Center)
        .into(),
        UpdateStatus::Available { version, url } => button(
            row![
                icons::arrow_repeat().size(13),
                text(format!("v{version} available")).size(13),
            ]
            .spacing(5)
            .align_y(iced::Alignment::Center),
        )
        .on_press(SettingsMessage::OpenReleasePage(url.clone()))
        .padding([2, 6])
        .style(theme::link_button)
        .into(),
        UpdateStatus::Failed => text("Couldn't check for updates")
            .size(13)
            .style(theme::secondary_text)
            .into(),
    };

    row![check, result]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
}

#[cfg(test)]
mod tests {
    use super::settings;
    use crate::app::SettingsMessage;
    use crate::config::AppConfig;
    use iced::Element;
    use iced_test::simulator;

    fn card<'a>(disk_cache_size: Option<u64>) -> Element<'a, SettingsMessage> {
        settings(
            &AppConfig::default(),
            disk_cache_size,
            false,
            #[cfg(feature = "update-check")]
            None,
        )
    }

    #[test]
    fn renders_title_and_steppers() {
        let mut ui = simulator(card(None));
        assert!(ui.find("Settings").is_ok());
        assert!(ui.find("Prefetch depth").is_ok());
        assert!(ui.find("Image cache budget").is_ok());
    }

    // Toggler labels aren't surfaced to `find`, so the disk-thumbs section is
    // checked through its findable store-size text.
    #[cfg(feature = "disk-thumbs")]
    #[test]
    fn shows_the_thumbnail_store_when_enabled() {
        let mut ui = simulator(card(Some(2048)));
        assert!(ui.find("Thumbnail store: 2.0 KB").is_ok());
    }

    #[cfg(not(feature = "disk-thumbs"))]
    #[test]
    fn hides_the_thumbnail_store_when_disabled() {
        let mut ui = simulator(card(Some(2048)));
        assert!(ui.find("Thumbnail store: 2.0 KB").is_err());
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn shows_version_and_check_button() {
        let mut ui = simulator(card(None));
        assert!(
            ui.find(concat!("scryglass v", env!("CARGO_PKG_VERSION")))
                .is_ok()
        );
        assert!(ui.find("Check for updates").is_ok());
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn shows_an_available_update_with_a_link() {
        use crate::update_check::UpdateStatus;
        let status = UpdateStatus::Available {
            version: "9.9.9".into(),
            url: "https://example/release".into(),
        };
        let mut ui = simulator(settings(&AppConfig::default(), None, false, Some(&status)));
        assert!(ui.find("v9.9.9 available").is_ok());
    }
}
