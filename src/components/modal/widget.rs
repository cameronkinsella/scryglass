//! Modal dialogs: confirm-delete and rename. Rendered as a centered
//! card over a scrim. Keyboard viewer interactions are gated off in
//! `update()` while one is open.

use iced::widget::{button, center, column, container, row, text, text_input};
use iced::{Element, Length};

use crate::app::ModalMessage;
use crate::ui::theme;

/// Stable ID so the rename input can be focused when the dialog opens.
pub fn rename_input_id() -> iced::widget::Id {
    iced::widget::Id::new("rename-input")
}

/// "Move to Recycle Bin?" confirmation.
pub fn confirm_delete<'a>(file_name: &str) -> Element<'a, ModalMessage> {
    let card = column![
        text("Move to Recycle Bin?").size(15),
        text(file_name.to_string())
            .size(13)
            .style(theme::secondary_text),
        row![
            button(text("Delete").size(13))
                .on_press(ModalMessage::ConfirmDeleteNow)
                .padding([5, 14])
                .style(button::danger),
            button(text("Cancel").size(13))
                .on_press(ModalMessage::Cancel)
                .padding([5, 14])
                .style(button::secondary),
        ]
        .spacing(8),
    ]
    .spacing(12)
    .padding(18);

    overlay(card.into())
}

/// Inline rename dialog with a focused text input.
pub fn rename_dialog<'a>(input: &str) -> Element<'a, ModalMessage> {
    let card = column![
        text("Rename").size(15),
        text_input("File name", input)
            .id(rename_input_id())
            .on_input(ModalMessage::RenameInput)
            .on_submit(ModalMessage::CommitRename)
            .size(13)
            .width(Length::Fixed(280.0)),
        row![
            button(text("Rename").size(13))
                .on_press(ModalMessage::CommitRename)
                .padding([5, 14])
                .style(button::primary),
            button(text("Cancel").size(13))
                .on_press(ModalMessage::Cancel)
                .padding([5, 14])
                .style(button::secondary),
        ]
        .spacing(8),
    ]
    .spacing(12)
    .padding(18);

    overlay(card.into())
}

fn overlay(card: Element<'_, ModalMessage>) -> Element<'_, ModalMessage> {
    center(container(card).style(theme::panel))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::{confirm_delete, rename_dialog, rename_input_id};
    use iced_test::simulator;

    #[test]
    fn confirm_delete_names_the_file_and_offers_choices() {
        let mut ui = simulator(confirm_delete("photo.jpg"));
        assert!(ui.find("Move to Recycle Bin?").is_ok());
        assert!(ui.find("photo.jpg").is_ok());
        assert!(ui.find("Delete").is_ok());
        assert!(ui.find("Cancel").is_ok());
    }

    #[test]
    fn rename_dialog_builds_with_a_stable_input_id() {
        let _ = rename_input_id();
        let mut ui = simulator(rename_dialog("photo.jpg"));
        assert!(ui.find("Cancel").is_ok());
    }
}
