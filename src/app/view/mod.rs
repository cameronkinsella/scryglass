use iced::widget::{Stack, column, mouse_area};
use iced::{Element, Length, window};

use super::{App, Envelope, Message, Shared, Window};
use crate::components::{
    context_menu, empty, modal, settings, toasts, toolbar, viewer, zoom_slider,
};

/// The daemon view for one window: render its UI, then tag every message it
/// emits with the window it came from.
pub fn view(app: &App, id: window::Id) -> Element<'_, Envelope> {
    let Some(win) = app.windows.get(&id) else {
        return iced::widget::column![]
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    };
    window_view(win, &app.shared).map(move |message| Envelope::Win(id, message))
}

fn window_view<'a>(win: &'a Window, shared: &'a Shared) -> Element<'a, Message> {
    let stacked = Stack::with_children(vec![
        viewer::view(win, shared),
        viewer::spinner(win, shared),
        zoom_slider::view(win, shared),
        toolbar::dropdown(win, shared),
        context_menu::view(win, shared),
        modal::view(win, shared),
        toasts::view(win, shared),
    ]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if !win.fullscreen && shared.config.standard.chrome.toolbar {
        page = page.push(toolbar::view(win, shared));
    }
    page = page.push(stacked);

    let base: Element<'_, Message> = if win.context_menu_pos.is_some() {
        mouse_area(page)
            .on_press(Message::ContextMenu(context_menu::Message::Dismiss))
            .on_right_press(Message::ContextMenu(context_menu::Message::Dismiss))
            .into()
    } else if win.open_menu.is_some() {
        mouse_area(page)
            .on_press(Message::Toolbar(toolbar::Message::DismissOverlay))
            .on_right_press(Message::Toolbar(toolbar::Message::DismissOverlay))
            .into()
    } else {
        mouse_area(page).into()
    };

    // Above the page so the dismiss backdrop covers the menu bar too.
    Stack::with_children(vec![base, help(win, shared), settings::view(win, shared)]).into()
}

fn help<'a>(win: &'a Window, _shared: &'a Shared) -> Element<'a, Message> {
    if win.help_open {
        crate::components::help::view()
    } else {
        empty()
    }
}

#[cfg(test)]
mod tests {
    use iced_test::simulator;

    use super::*;
    use crate::app::test_support::{empty_app, into_app};

    #[test]
    fn empty_app_renders_the_drop_prompt() {
        let (app, id) = into_app(empty_app());
        let mut ui = simulator(view(&app, id));
        assert!(
            ui.find("Drop an image here to begin").is_ok(),
            "the empty viewer should show the drop prompt"
        );
    }
}
