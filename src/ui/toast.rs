//! Toast notifications: transient messages stacked bottom-center.
//!
//! Errors that previously vanished silently (decode failures, unreadable
//! directories) surface here without interrupting the viewer.

use iced::widget::{column, container, text};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::ui::theme;

/// What flavor of message a toast carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

/// A transient notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub text: String,
}

/// Render the toast stack, anchored bottom-center above the chrome.
pub fn toast_stack<'a>(toasts: &'a [Toast]) -> Element<'a, Message> {
    let cards = toasts.iter().map(|toast| {
        let style = match toast.kind {
            ToastKind::Info => theme::toast_info,
            ToastKind::Error => theme::toast_error,
        };
        container(text(&toast.text).size(13))
            .padding([8, 14])
            .style(style)
            .into()
    });

    container(column(cards).spacing(8).align_x(Alignment::Center))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::End)
        .padding(24)
        .into()
}
