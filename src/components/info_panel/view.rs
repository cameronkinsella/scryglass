//! Info panel: file details and EXIF metadata beside the image.

use iced::widget::{column, container, rule, scrollable, text};
use iced::{Element, Length};

use crate::app::Message;
use crate::ui::theme;

/// Width of the panel in logical pixels (the viewport math subtracts it).
pub const WIDTH: f32 = 280.0;

/// Render the info panel. `exif` is `None` while the probe is still running.
pub fn info_panel<'a>(
    file_name: &str,
    details: &[(String, String)],
    exif: Option<&'a [(String, String)]>,
) -> Element<'a, Message> {
    let entry = |label: &str, value: &str| {
        column![
            text(label.to_string())
                .size(11)
                .style(theme::secondary_text),
            text(value.to_string()).size(13),
        ]
        .spacing(2)
    };

    let mut rows = column![text(file_name.to_string()).size(14)]
        .spacing(10)
        .padding(14);

    for (label, value) in details {
        rows = rows.push(entry(label, value));
    }

    rows = rows.push(rule::horizontal(1));

    match exif {
        None => {
            rows = rows.push(text("…").size(13).style(theme::secondary_text));
        }
        Some([]) => {
            rows = rows.push(
                text("No camera metadata")
                    .size(13)
                    .style(theme::secondary_text),
            );
        }
        Some(fields) => {
            for (label, value) in fields {
                rows = rows.push(entry(label, value));
            }
        }
    }

    container(scrollable(rows).width(Length::Fixed(WIDTH)))
        .height(Length::Fill)
        .style(theme::surface)
        .into()
}

#[cfg(test)]
mod tests {
    use super::info_panel;
    use iced_test::simulator;

    fn details() -> Vec<(String, String)> {
        vec![("Dimensions".to_string(), "256 × 512 pixels".to_string())]
    }

    #[test]
    fn shows_file_name_and_details_while_exif_pending() {
        let mut ui = simulator(info_panel("photo.jpg", &details(), None));
        assert!(ui.find("photo.jpg").is_ok());
        assert!(ui.find("256 × 512 pixels").is_ok());
    }

    #[test]
    fn notes_when_there_is_no_camera_metadata() {
        let mut ui = simulator(info_panel("photo.jpg", &details(), Some(&[])));
        assert!(ui.find("No camera metadata").is_ok());
    }

    #[test]
    fn lists_exif_fields_when_present() {
        let exif = [("Camera".to_string(), "Acme One".to_string())];
        let mut ui = simulator(info_panel("photo.jpg", &details(), Some(&exif)));
        assert!(ui.find("Acme One").is_ok());
    }
}
