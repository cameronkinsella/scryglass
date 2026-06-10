//! Footer widget: image info on the left, directory position on the right.

use iced::widget::{row, space, text};
use iced::{Alignment, Element, Length};

use crate::app::Message;

/// Render the bottom footer bar.
///
/// Left side: image dimensions + file size (with icons).
/// Right side: zoom percentage + position in directory (with icons).
///
/// Each item has a minimum width so positions stay stable across
/// typical values. Unusually large values will push items apart.
pub fn footer<'a>(
    dimensions: &str,
    file_size: &str,
    zoom_pct: u32,
    position: &str,
) -> Element<'a, Message> {
    use iced_fonts::bootstrap;

    let dimensions_item = row![
        bootstrap::aspect_ratio().size(13),
        text(format!(" {dimensions}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(145.0));

    let file_size_item = row![
        bootstrap::hdd().size(13),
        text(format!(" {file_size}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(90.0));

    let zoom_item = row![
        bootstrap::zoom_in().size(13),
        text(format!(" {zoom_pct}%")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(70.0));

    let position_item = row![
        bootstrap::images().size(13),
        text(format!(" {position}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(70.0));

    row![
        dimensions_item,
        file_size_item,
        space::horizontal(),
        zoom_item,
        position_item,
    ]
    .align_y(Alignment::Center)
    .padding([4, 12])
    .into()
}
