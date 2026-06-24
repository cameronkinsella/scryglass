use iced::widget::{Stack, column, mouse_area};
use iced::{Element, Length};

use super::{App, Message};
use crate::components::{
    context_menu, empty, modal, settings, toasts, toolbar, viewer, zoom_slider,
};

pub fn view(app: &App) -> Element<'_, Message> {
    let stacked = Stack::with_children(vec![
        viewer::view(app),
        viewer::spinner(app),
        zoom_slider::view(app),
        toolbar::dropdown(app),
        context_menu::view(app),
        modal::view(app),
        toasts::view(app),
    ]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if !app.fullscreen && app.config.show_toolbar {
        page = page.push(toolbar::view(app));
    }
    page = page.push(stacked);

    let base: Element<'_, Message> = if app.context_menu_pos.is_some() {
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
    };

    // Above the page so the dismiss backdrop covers the menu bar too.
    Stack::with_children(vec![base, help(app), settings::view(app)]).into()
}

fn help(app: &App) -> Element<'_, Message> {
    if app.help_open {
        crate::components::help::view()
    } else {
        empty()
    }
}

#[cfg(test)]
mod tests {
    use iced_test::simulator;

    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn empty_app_renders_the_drop_prompt() {
        let app = empty_app();
        let mut ui = simulator(view(&app));
        assert!(
            ui.find("Drop an image here to begin").is_ok(),
            "the empty viewer should show the drop prompt"
        );
    }
}
