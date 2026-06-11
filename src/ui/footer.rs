//! Footer widget: image info on the left, directory position on the right.

use iced::widget::{container, row, space, text};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::ui::theme;

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
    zoom: &str,
    position: &str,
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

    let zoom_item = row![icons::zoom_in().size(13), text(format!(" {zoom}")).size(13),]
        .align_y(Alignment::Center)
        .width(Length::Fixed(70.0));

    let position_item = row![
        icons::images().size(13),
        text(format!(" {position}")).size(13),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fixed(70.0));

    let bar = row![
        dimensions_item,
        file_size_item,
        space::horizontal(),
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
