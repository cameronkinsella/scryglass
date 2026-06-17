use crate::config::{SortKey, ZoomMode};

#[derive(Debug, Clone)]
pub enum Message {
    ToggleFileMenu,
    ToggleZoomMenu,
    ToggleLayoutMenu,
    ToggleSortMenu,
    SetSortKey(SortKey),
    ToggleSortDirection,
    DismissOverlay,
    SetZoomMode(ZoomMode),
    ToggleFilmstrip,
    ToggleSlider,
    ToggleFooter,
    ToggleToolbar,
    ToggleTheme,
    ToggleCrispPixels,
}
use iced::widget::column;
use iced::{Element, Length, Task};

use crate::app::update::{fire_resort, save_config};
use crate::app::{App, Message as AppMessage, recalc_viewport};
use crate::components::empty;
use crate::config::ThemeChoice;
use widget::LayoutVisibility;

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    widget::menu_bar(app.open_menu)
}

pub(crate) fn dropdown(app: &App) -> Element<'_, AppMessage> {
    let layout_vis = LayoutVisibility {
        show_filmstrip: app.config.show_filmstrip,
        show_slider: app.config.show_slider,
        show_footer: app.config.show_footer,
        show_info: app.config.show_info,
        show_checkerboard: app.config.show_checkerboard,
    };

    if let Some(dropdown) = widget::dropdown(
        app.open_menu,
        app.config.zoom_mode,
        layout_vis,
        app.config.theme == ThemeChoice::Light,
        app.config.crisp_pixels,
        app.config.sort_key,
        app.config.sort_desc,
    ) {
        column![dropdown]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        empty()
    }
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::ToggleFileMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::File) {
                None
            } else {
                Some(OpenMenu::File)
            };
            Task::none()
        }
        Message::ToggleZoomMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Zoom) {
                None
            } else {
                Some(OpenMenu::Zoom)
            };
            Task::none()
        }
        Message::ToggleLayoutMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Layout) {
                None
            } else {
                Some(OpenMenu::Layout)
            };
            Task::none()
        }
        Message::ToggleSortMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Sort) {
                None
            } else {
                Some(OpenMenu::Sort)
            };
            Task::none()
        }
        Message::SetSortKey(key) => {
            app.open_menu = None;
            app.config.sort_key = key;
            Task::batch([save_config(app), fire_resort(app)])
        }
        Message::ToggleSortDirection => {
            app.config.sort_desc = !app.config.sort_desc;
            Task::batch([save_config(app), fire_resort(app)])
        }
        Message::DismissOverlay => {
            app.open_menu = None;
            Task::none()
        }
        Message::SetZoomMode(mode) => {
            app.open_menu = None;
            app.config.zoom_mode = mode;
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            if let Some(viewer) = app.viewer_mut() {
                viewer.manual_zoom = false;
                if let Some((w, h)) = viewer.displayed.original_size() {
                    viewer.zoom = crate::app::viewer_math::compute_zoom(zoom_mode, w, h, viewport);
                    viewer.pan = (0.0, 0.0);
                }
            }
            save_config(app)
        }
        Message::ToggleFilmstrip => {
            app.config.show_filmstrip = !app.config.show_filmstrip;
            recalc_viewport(app);
            save_config(app)
        }
        Message::ToggleSlider => {
            app.config.show_slider = !app.config.show_slider;
            recalc_viewport(app);
            save_config(app)
        }
        Message::ToggleFooter => {
            app.config.show_footer = !app.config.show_footer;
            recalc_viewport(app);
            save_config(app)
        }
        Message::ToggleToolbar => {
            app.config.show_toolbar = !app.config.show_toolbar;
            app.context_menu_pos = None;
            recalc_viewport(app);
            save_config(app)
        }
        Message::ToggleTheme => {
            app.config.theme = match app.config.theme {
                ThemeChoice::Dark => ThemeChoice::Light,
                ThemeChoice::Light => ThemeChoice::Dark,
            };
            save_config(app)
        }
        Message::ToggleCrispPixels => {
            app.config.crisp_pixels = !app.config.crisp_pixels;
            save_config(app)
        }
    }
}
mod widget;

pub(crate) use widget::OpenMenu;
