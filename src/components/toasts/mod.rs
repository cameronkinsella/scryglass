#[derive(Debug, Clone)]
pub enum Message {
    Dismiss(u64),
}
use iced::{Element, Task};

use crate::app::{App, Message as AppMessage};

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    widget::toast_stack(&app.toasts)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Dismiss(id) => {
            app.toasts.retain(|t| t.id != id);
            Task::none()
        }
    }
}
mod widget;

pub(crate) use widget::{Toast, ToastKind};
