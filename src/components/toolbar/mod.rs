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
use crate::app::{Message as AppMessage, Shared, Window, recalc_viewport};
use crate::components::empty;
use crate::config::ThemeChoice;
use widget::LayoutVisibility;

pub(crate) fn view<'a>(win: &'a Window, _shared: &'a Shared) -> Element<'a, AppMessage> {
    widget::menu_bar(win.open_menu)
}

pub(crate) fn dropdown<'a>(win: &'a Window, shared: &'a Shared) -> Element<'a, AppMessage> {
    let layout_vis = LayoutVisibility {
        show_filmstrip: shared.config.show_filmstrip,
        show_slider: shared.config.show_slider,
        show_footer: shared.config.show_footer,
        show_info: shared.config.show_info,
        show_checkerboard: shared.config.show_checkerboard,
    };

    if let Some(dropdown) = widget::dropdown(
        win.open_menu,
        shared.config.zoom_mode,
        layout_vis,
        shared.config.theme == ThemeChoice::Light,
        shared.config.crisp_pixels,
        shared.config.sort_key,
        shared.config.sort_desc,
    ) {
        column![dropdown]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    } else {
        empty()
    }
}

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::ToggleFileMenu => {
            win.open_menu = if win.open_menu == Some(OpenMenu::File) {
                None
            } else {
                Some(OpenMenu::File)
            };
            Task::none()
        }
        Message::ToggleZoomMenu => {
            win.open_menu = if win.open_menu == Some(OpenMenu::Zoom) {
                None
            } else {
                Some(OpenMenu::Zoom)
            };
            Task::none()
        }
        Message::ToggleLayoutMenu => {
            win.open_menu = if win.open_menu == Some(OpenMenu::Layout) {
                None
            } else {
                Some(OpenMenu::Layout)
            };
            Task::none()
        }
        Message::ToggleSortMenu => {
            win.open_menu = if win.open_menu == Some(OpenMenu::Sort) {
                None
            } else {
                Some(OpenMenu::Sort)
            };
            Task::none()
        }
        Message::SetSortKey(key) => {
            shared.config.sort_key = key;
            Task::batch([save_config(win, shared), fire_resort(win, shared)])
        }
        Message::ToggleSortDirection => {
            shared.config.sort_desc = !shared.config.sort_desc;
            Task::batch([save_config(win, shared), fire_resort(win, shared)])
        }
        Message::DismissOverlay => {
            win.open_menu = None;
            Task::none()
        }
        Message::SetZoomMode(mode) => {
            shared.config.zoom_mode = mode;
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            if let Some(viewer) = win.viewer_mut() {
                viewer.manual_zoom = false;
                if let Some((w, h)) = viewer.displayed.original_size() {
                    viewer.zoom = crate::app::viewer_math::compute_zoom(zoom_mode, w, h, viewport);
                    viewer.pan = (0.0, 0.0);
                }
            }
            save_config(win, shared)
        }
        Message::ToggleFilmstrip => {
            shared.config.show_filmstrip = !shared.config.show_filmstrip;
            recalc_viewport(win, shared);
            let saved = save_config(win, shared);
            if !shared.config.show_filmstrip {
                return saved;
            }
            // Showing the strip mid-session: position it on the cursor, as
            // opening the directory with the strip already on would.
            let window_w = win.window_size.width;
            let Some(viewer) = win.viewer_mut() else {
                return saved;
            };
            let offset = crate::components::filmstrip::open_offset(
                viewer.nav.cursor(),
                window_w,
                viewer.nav.len(),
            );
            viewer.filmstrip_scroll_x = offset;
            let scroll = iced::widget::operation::scroll_to(
                crate::components::filmstrip::filmstrip_id(win.id),
                iced::widget::scrollable::AbsoluteOffset { x: offset, y: 0.0 },
            );
            Task::batch([saved, scroll])
        }
        Message::ToggleSlider => {
            shared.config.show_slider = !shared.config.show_slider;
            recalc_viewport(win, shared);
            save_config(win, shared)
        }
        Message::ToggleFooter => {
            shared.config.show_footer = !shared.config.show_footer;
            recalc_viewport(win, shared);
            save_config(win, shared)
        }
        Message::ToggleToolbar => {
            shared.config.show_toolbar = !shared.config.show_toolbar;
            win.context_menu_pos = None;
            recalc_viewport(win, shared);
            save_config(win, shared)
        }
        Message::ToggleTheme => {
            shared.config.theme = match shared.config.theme {
                ThemeChoice::Dark => ThemeChoice::Light,
                ThemeChoice::Light => ThemeChoice::Dark,
            };
            save_config(win, shared)
        }
        Message::ToggleCrispPixels => {
            shared.config.crisp_pixels = !shared.config.crisp_pixels;
            save_config(win, shared)
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
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFileMenu);
        assert!(app.window.open_menu == Some(OpenMenu::File));
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFileMenu);
        assert!(app.window.open_menu.is_none());
    }

    #[test]
    fn opening_a_second_menu_replaces_the_first() {
        let mut app = empty_app();
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFileMenu);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleZoomMenu);
        assert!(app.window.open_menu == Some(OpenMenu::Zoom));
    }

    #[test]
    fn dismiss_overlay_closes_any_menu() {
        let mut app = empty_app();
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleLayoutMenu);
        let _ = update(&mut app.window, &mut app.shared, Message::DismissOverlay);
        assert!(app.window.open_menu.is_none());
    }

    #[test]
    fn showing_the_filmstrip_positions_it_on_the_cursor() {
        let names: Vec<String> = (0..1000).map(|i| format!("{i:04}.png")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut app = viewing_app(&refs, 600);
        app.shared.config.show_filmstrip = false;
        app.viewer_mut().unwrap().filmstrip_scroll_x = 0.0;
        let window_w = app.window.window_size.width;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFilmstrip);
        // Lands where opening at 600 with the strip on would, not at the start.
        let expected = crate::components::filmstrip::open_offset(600, window_w, 1000);
        assert_eq!(app.viewer().unwrap().filmstrip_scroll_x, expected);
        assert!(expected > 0.0);
    }

    #[test]
    fn layout_toggles_flip_their_config_flags() {
        let mut app = empty_app();
        let (filmstrip, slider, footer) = (
            app.shared.config.show_filmstrip,
            app.shared.config.show_slider,
            app.shared.config.show_footer,
        );
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFilmstrip);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleSlider);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFooter);
        assert_eq!(app.shared.config.show_filmstrip, !filmstrip);
        assert_eq!(app.shared.config.show_slider, !slider);
        assert_eq!(app.shared.config.show_footer, !footer);
    }

    #[test]
    fn toggle_toolbar_flips_and_dismisses_the_context_menu() {
        let mut app = empty_app();
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let before = app.shared.config.show_toolbar;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleToolbar);
        assert_eq!(app.shared.config.show_toolbar, !before);
        assert!(app.window.context_menu_pos.is_none());
    }

    #[test]
    fn toggle_theme_swaps_dark_and_light() {
        let mut app = empty_app();
        app.shared.config.theme = ThemeChoice::Dark;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleTheme);
        assert_eq!(app.shared.config.theme, ThemeChoice::Light);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleTheme);
        assert_eq!(app.shared.config.theme, ThemeChoice::Dark);
    }

    #[test]
    fn toggle_crisp_pixels_flips_config() {
        let mut app = empty_app();
        let before = app.shared.config.crisp_pixels;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleCrispPixels);
        assert_eq!(app.shared.config.crisp_pixels, !before);
    }

    #[test]
    fn set_zoom_mode_keeps_the_menu_open_and_clears_manual_zoom() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.open_menu = Some(OpenMenu::Zoom);
        app.viewer_mut().unwrap().manual_zoom = true;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::SetZoomMode(ZoomMode::default()),
        );
        assert_eq!(app.window.open_menu, Some(OpenMenu::Zoom));
        assert_eq!(app.shared.config.zoom_mode, ZoomMode::default());
        assert!(!app.viewer().unwrap().manual_zoom);
    }

    #[test]
    fn set_sort_key_keeps_the_menu_open_and_records_the_key() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.open_menu = Some(OpenMenu::Sort);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::SetSortKey(SortKey::default()),
        );
        assert_eq!(app.window.open_menu, Some(OpenMenu::Sort));
        assert_eq!(app.shared.config.sort_key, SortKey::default());
    }

    #[test]
    fn keeping_the_menu_open_is_a_noop() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.open_menu = Some(OpenMenu::Layout);
        let _ = update(&mut app.window, &mut app.shared, Message::KeepMenuOpen);
        assert_eq!(app.window.open_menu, Some(OpenMenu::Layout));
    }

    #[test]
    fn toggle_sort_direction_flips_config() {
        let mut app = viewing_app(&["a.png"], 0);
        let before = app.shared.config.sort_desc;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ToggleSortDirection,
        );
        assert_eq!(app.shared.config.sort_desc, !before);
    }
}
