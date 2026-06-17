mod view;

use std::time::Duration;

use iced::Element;

use crate::app::Message;

pub(crate) fn view<'a>(
    dimensions: &str,
    file_size: &str,
    zoom: &str,
    position: &str,
    loading: Option<Duration>,
) -> Element<'a, Message> {
    view::footer(dimensions, file_size, zoom, position, loading)
}
