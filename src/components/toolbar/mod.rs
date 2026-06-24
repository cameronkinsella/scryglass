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
    /// No-op: swallows a click on a dropdown's surface so it doesn't dismiss.
    KeepMenuOpen,
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
        Message::KeepMenuOpen => Task::none(),
    }
}
mod widget;

pub(crate) use widget::OpenMenu;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, viewing_app};

    #[test]
    fn file_menu_toggles_open_and_closed() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::ToggleFileMenu);
        assert!(app.open_menu == Some(OpenMenu::File));
        let _ = update(&mut app, Message::ToggleFileMenu);
        assert!(app.open_menu.is_none());
    }

    #[test]
    fn opening_a_second_menu_replaces_the_first() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::ToggleFileMenu);
        let _ = update(&mut app, Message::ToggleZoomMenu);
        assert!(app.open_menu == Some(OpenMenu::Zoom));
    }

    #[test]
    fn dismiss_overlay_closes_any_menu() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::ToggleLayoutMenu);
        let _ = update(&mut app, Message::DismissOverlay);
        assert!(app.open_menu.is_none());
    }

    #[test]
    fn layout_toggles_flip_their_config_flags() {
        let mut app = empty_app();
        let (filmstrip, slider, footer) = (
            app.config.show_filmstrip,
            app.config.show_slider,
            app.config.show_footer,
        );
        let _ = update(&mut app, Message::ToggleFilmstrip);
        let _ = update(&mut app, Message::ToggleSlider);
        let _ = update(&mut app, Message::ToggleFooter);
        assert_eq!(app.config.show_filmstrip, !filmstrip);
        assert_eq!(app.config.show_slider, !slider);
        assert_eq!(app.config.show_footer, !footer);
    }

    #[test]
    fn toggle_toolbar_flips_and_dismisses_the_context_menu() {
        let mut app = empty_app();
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let before = app.config.show_toolbar;
        let _ = update(&mut app, Message::ToggleToolbar);
        assert_eq!(app.config.show_toolbar, !before);
        assert!(app.context_menu_pos.is_none());
    }

    #[test]
    fn toggle_theme_swaps_dark_and_light() {
        let mut app = empty_app();
        app.config.theme = ThemeChoice::Dark;
        let _ = update(&mut app, Message::ToggleTheme);
        assert_eq!(app.config.theme, ThemeChoice::Light);
        let _ = update(&mut app, Message::ToggleTheme);
        assert_eq!(app.config.theme, ThemeChoice::Dark);
    }

    #[test]
    fn toggle_crisp_pixels_flips_config() {
        let mut app = empty_app();
        let before = app.config.crisp_pixels;
        let _ = update(&mut app, Message::ToggleCrispPixels);
        assert_eq!(app.config.crisp_pixels, !before);
    }

    #[test]
    fn set_zoom_mode_keeps_the_menu_open_and_clears_manual_zoom() {
        let mut app = viewing_app(&["a.png"], 0);
        app.open_menu = Some(OpenMenu::Zoom);
        app.viewer_mut().unwrap().manual_zoom = true;
        let _ = update(&mut app, Message::SetZoomMode(ZoomMode::default()));
        assert_eq!(app.open_menu, Some(OpenMenu::Zoom));
        assert_eq!(app.config.zoom_mode, ZoomMode::default());
        assert!(!app.viewer().unwrap().manual_zoom);
    }

    #[test]
    fn set_sort_key_keeps_the_menu_open_and_records_the_key() {
        let mut app = viewing_app(&["a.png"], 0);
        app.open_menu = Some(OpenMenu::Sort);
        let _ = update(&mut app, Message::SetSortKey(SortKey::default()));
        assert_eq!(app.open_menu, Some(OpenMenu::Sort));
        assert_eq!(app.config.sort_key, SortKey::default());
    }

    #[test]
    fn keeping_the_menu_open_is_a_noop() {
        let mut app = viewing_app(&["a.png"], 0);
        app.open_menu = Some(OpenMenu::Layout);
        let _ = update(&mut app, Message::KeepMenuOpen);
        assert_eq!(app.open_menu, Some(OpenMenu::Layout));
    }

    #[test]
    fn toggle_sort_direction_flips_config() {
        let mut app = viewing_app(&["a.png"], 0);
        let before = app.config.sort_desc;
        let _ = update(&mut app, Message::ToggleSortDirection);
        assert_eq!(app.config.sort_desc, !before);
    }
}
