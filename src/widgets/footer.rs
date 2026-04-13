//! Footer widget: image info on the left, directory position on the right.

use iced::Element;
use iced::widget::{row, space, text};

use crate::app::Message;

/// Render the bottom footer bar.
///
/// Left side: image dimensions + file size.
/// Right side: position in directory (e.g. "3/48").
pub fn footer<'a>(dimensions: &str, file_size: &str, position: &str) -> Element<'a, Message> {
    let left = format!("{dimensions}    {file_size}");
    let right = position.to_string();
    row![
        text(left).size(13),
        space::horizontal(),
        text(right).size(13),
    ]
    .padding([4, 12])
    .into()
}
