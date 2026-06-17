mod view;

use iced::Element;

use crate::app::Message;

pub(crate) fn view<'a>() -> Element<'a, Message> {
    view::help_overlay()
}
