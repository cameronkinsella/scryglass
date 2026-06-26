#[derive(Debug, Clone)]
pub enum Message {
    Dismiss(u64),
}
use iced::{Element, Task};

use crate::app::{Message as AppMessage, Shared, Window};

pub(crate) fn view<'a>(win: &'a Window, _shared: &'a Shared) -> Element<'a, AppMessage> {
    widget::toast_stack(&win.toasts)
}

pub(crate) fn update(win: &mut Window, _shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Dismiss(id) => {
            win.toasts.retain(|t| t.id != id);
            Task::none()
        }
    }
}
mod widget;

pub(crate) use widget::{Toast, ToastKind};
