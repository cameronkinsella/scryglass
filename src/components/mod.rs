pub mod context_menu;
pub mod filmstrip;
pub mod footer;
pub mod help;
pub mod info_panel;
pub mod modal;
pub mod nav_slider;
pub mod settings;
pub mod toasts;
pub mod toolbar;
pub mod video_controls;
pub mod viewer;
pub mod zoom_slider;

pub(crate) fn empty<'a, Message: 'a>() -> iced::Element<'a, Message> {
    iced::widget::column![]
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .into()
}
