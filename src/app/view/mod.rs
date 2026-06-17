use iced::widget::{Stack, column, mouse_area};
use iced::{Element, Length};

use super::{App, Message};
use crate::components::{
    context_menu, empty, modal, nav_slider, settings, toasts, toolbar, viewer,
};

pub fn view(app: &App) -> Element<'_, Message> {
    let stacked = Stack::with_children(vec![
        viewer::view(app),
        viewer::spinner(app),
        nav_slider::scrub_bubble(app),
        toolbar::dropdown(app),
        context_menu::view(app),
        help(app),
        modal::view(app),
        settings::view(app),
        toasts::view(app),
    ]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if !app.fullscreen && app.config.show_toolbar {
        page = page.push(toolbar::view(app));
    }
    page = page.push(stacked);

    if app.context_menu_pos.is_some() {
        mouse_area(page)
            .on_press(Message::ContextMenu(context_menu::Message::Dismiss))
            .on_right_press(Message::ContextMenu(context_menu::Message::Dismiss))
            .into()
    } else if app.open_menu.is_some() {
        mouse_area(page)
            .on_press(Message::Toolbar(toolbar::Message::DismissOverlay))
            .on_right_press(Message::Toolbar(toolbar::Message::DismissOverlay))
            .into()
    } else {
        mouse_area(page).into()
    }
}

fn help(app: &App) -> Element<'_, Message> {
    if app.help_open {
        crate::components::help::view()
    } else {
        empty()
    }
}
