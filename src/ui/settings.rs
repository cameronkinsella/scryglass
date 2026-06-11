//! Settings dialog: the persisted knobs that don't have a menu home.

use iced::widget::{button, center, column, container, row, rule, text, toggler};
use iced::{Element, Length};

use crate::app::Message;
use crate::config::AppConfig;
use crate::ui::theme;

/// Render the settings card.
///
/// `disk_cache_size` is the probed size of the persistent thumbnail
/// store (`None` while probing).
pub fn settings<'a>(config: &AppConfig, disk_cache_size: Option<u64>) -> Element<'a, Message> {
    let switch = |label: &str, on: bool, msg: fn(bool) -> Message| {
        toggler(on)
            .label(label.to_string())
            .on_toggle(msg)
            .size(15)
            .text_size(13)
            .width(Length::Fill)
    };

    let stepper = |label: &str, value: String, dec: Option<Message>, inc: Option<Message>| {
        let small = |t: &str, msg: Option<Message>| {
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
        text("Settings").size(16),
        switch(
            "Read-only mode (no delete or rename)",
            config.read_only,
            |_| { Message::ToggleReadOnly }
        ),
        switch("Confirm before deleting", config.confirm_delete, |_| {
            Message::ToggleConfirmDelete
        }),
        rule::horizontal(1),
        stepper(
            "Prefetch depth",
            depth.to_string(),
            (depth > 1).then(|| Message::SetPrefetchDepth(depth - 1)),
            (depth < 10).then(|| Message::SetPrefetchDepth(depth + 1)),
        ),
        stepper(
            "Image cache budget",
            format!("{budget} MB"),
            (budget > 128).then(|| Message::SetCacheBudget(budget - 128)),
            (budget < 4096).then(|| Message::SetCacheBudget(budget + 128)),
        ),
        rule::horizontal(1),
        switch("Persistent thumbnails", config.disk_thumbs, |_| {
            Message::ToggleDiskThumbs
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
                .on_press(Message::ClearDiskThumbs)
                .padding([3, 12])
                .style(button::secondary),
        ]
        .align_y(iced::Alignment::Center),
    );

    rows = rows.push(rule::horizontal(1));
    rows = rows.push(
        container(
            button(text("Close").size(13))
                .on_press(Message::ModalCancel)
                .padding([4, 16])
                .style(button::primary),
        )
        .width(Length::Fill)
        .align_x(iced::Alignment::End),
    );

    center(container(rows).style(theme::panel))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
