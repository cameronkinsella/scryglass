//! View function: assembles toolbar, content area, overlays, and footer.

mod content;
mod overlays;

use iced::widget::{Stack, column, mouse_area};
use iced::{Element, Length};

use super::{App, Message};

/// View function: assembles toolbar, content area, and footer.
pub fn view(app: &App) -> Element<'_, Message> {
    let stacked = Stack::with_children(vec![
        content::content(app),
        overlays::spinner(app),
        overlays::scrub_bubble(app),
        overlays::toolbar_dropdown(app),
        overlays::context_menu(app),
        overlays::help(app),
        overlays::modal(app),
        overlays::toasts(app),
    ]);

    let mut page = column![].width(Length::Fill).height(Length::Fill);

    if !app.fullscreen && app.config.show_toolbar {
        page = page.push(crate::ui::toolbar::menu_bar(app.open_menu));
    }
    page = page.push(stacked);

    if app.context_menu_pos.is_some() {
        mouse_area(page)
            .on_press(Message::DismissContextMenu)
            .on_right_press(Message::DismissContextMenu)
            .into()
    } else if app.open_menu.is_some() {
        mouse_area(page)
            .on_press(Message::DismissOverlay)
            .on_right_press(Message::DismissOverlay)
            .into()
    } else {
        mouse_area(page).into()
    }
}
