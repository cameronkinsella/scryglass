mod view;

use iced::Element;

use crate::app::Message;

pub(crate) use view::WIDTH;

pub(crate) fn view<'a>(
    file_name: &str,
    details: &[(String, String)],
    exif: Option<&'a [(String, String)]>,
) -> Element<'a, Message> {
    view::info_panel(file_name, details, exif)
}
