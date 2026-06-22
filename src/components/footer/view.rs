//! Footer widget: image info on the left, directory position on the right.

use std::time::Duration;

use iced::widget::{button, container, row, space, text};
use iced::{Alignment, Element, Length};

use crate::app::{Message, ViewerMessage};
use crate::ui::theme;

/// Render the footer bar. Items have minimum widths so positions stay
/// stable as values change.
pub fn footer<'a>(
    dimensions: &str,
    file_size: &str,
    zoom: &str,
    position: &str,
    loading: Option<Duration>,
) -> Element<'a, Message> {
    use crate::ui::icons;

    let dimensions_item = row![
        icons::aspect_ratio().size(13),
        text(format!(" {dimensions}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(145.0));

    let file_size_item = row![
        icons::hdd().size(13),
        text(format!(" {file_size}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(90.0));

    // Fixed-width container so the footer doesn't shift as the % changes.
    let zoom_item = container(
        button(
            row![icons::zoom_in().size(13), text(zoom.to_string()).size(13)]
                .spacing(4)
                .align_y(Alignment::Center),
        )
        .on_press(Message::Viewer(ViewerMessage::ToggleZoomSlider))
        .padding([2, 6])
        .style(theme::menu_item),
    )
    .width(Length::Fixed(74.0));

    let position_item = row![
        icons::images().size(13),
        text(format!(" {position}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(70.0));

    // Loading indicator lives down here so it never covers the image.
    let loading_item: Element<'a, Message> = match loading {
        Some(elapsed) => crate::ui::spinner::spinner_sized(elapsed, 14.0),
        None => space::horizontal().width(Length::Fixed(14.0)).into(),
    };

    let bar = row![
        dimensions_item,
        file_size_item,
        space::horizontal(),
        loading_item,
        space::horizontal().width(Length::Fixed(14.0)),
        zoom_item,
        position_item,
    ]
    .align_y(Alignment::Center)
    .padding([4, 12]);

    container(bar)
        .width(Length::Fill)
        .style(theme::surface)
        .into()
}

#[cfg(test)]
mod tests {
    use super::footer;
    use iced_test::simulator;
    use std::time::Duration;

    #[test]
    fn shows_the_zoom_readout() {
        let mut ui = simulator(footer("256 × 512 pixels", "1.2 MB", "100%", "3 / 10", None));
        assert!(ui.find("100%").is_ok());
    }

    #[test]
    fn renders_with_a_loading_spinner() {
        let mut ui = simulator(footer(
            "…",
            "…",
            "50%",
            "1 / 1",
            Some(Duration::from_millis(500)),
        ));
        assert!(ui.find("50%").is_ok());
    }
}
