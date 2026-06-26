use iced::Size;

#[derive(Debug, Clone)]
pub enum Message {
    Resized(Size),
    /// Window state fetched after a resize, to persist only a windowed size.
    WindowState {
        size: Size,
        maximized: bool,
        mode: iced::window::Mode,
    },
    CloseRequested(iced::window::Id),
}
use iced::Task;

use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{App, Message as AppMessage, recalc_viewport};

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Resized(size) => {
            app.window_size = size;
            recalc_viewport(app);
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;

            if let Some(viewer) = app.viewer_mut()
                && let Some((w, h)) = viewer.displayed.original_size()
            {
                if !viewer.manual_zoom {
                    viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
                }
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }

            // The app's own fullscreen never persists. A natively maximized or
            // fullscreened window looks like any other resize here, so confirm
            // the state before persisting and let the windowed size stand.
            if app.fullscreen {
                Task::none()
            } else {
                check_window_state(size)
            }
        }

        Message::WindowState {
            size,
            maximized,
            mode,
        } => {
            if should_persist(maximized, mode) {
                app.config.window_width = size.width;
                app.config.window_height = size.height;
            }
            Task::none()
        }

        Message::CloseRequested(id) => {
            let config = app.config.clone();
            Task::future(config.save()).then(move |_| iced::window::close(id))
        }
    }
}

/// Ask the windowing system whether the window is maximized or fullscreen, so
/// the size is persisted only when it is the plain windowed size.
fn check_window_state(size: Size) -> Task<AppMessage> {
    iced::window::latest().and_then(move |id| {
        iced::window::is_maximized(id).then(move |maximized| {
            iced::window::mode(id).map(move |mode| {
                AppMessage::Window(Message::WindowState {
                    size,
                    maximized,
                    mode,
                })
            })
        })
    })
}

/// A size worth remembering only when the window is neither maximized nor
/// fullscreen.
fn should_persist(maximized: bool, mode: iced::window::Mode) -> bool {
    !maximized && mode == iced::window::Mode::Windowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn resize_updates_the_window_size() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::Resized(Size::new(1024.0, 768.0)));
        assert_eq!(app.window_size, Size::new(1024.0, 768.0));
    }

    #[test]
    fn a_normal_window_state_persists_its_size() {
        let mut app = empty_app();
        let _ = update(
            &mut app,
            Message::WindowState {
                size: Size::new(1024.0, 768.0),
                maximized: false,
                mode: iced::window::Mode::Windowed,
            },
        );
        assert_eq!(app.config.window_width, 1024.0);
        assert_eq!(app.config.window_height, 768.0);
    }

    #[test]
    fn a_maximized_or_fullscreen_state_keeps_the_windowed_size() {
        let mut app = empty_app();
        let _ = update(
            &mut app,
            Message::WindowState {
                size: Size::new(1024.0, 768.0),
                maximized: false,
                mode: iced::window::Mode::Windowed,
            },
        );
        // Later maximized or fullscreen reports must not overwrite it.
        for state in [
            (true, iced::window::Mode::Windowed),
            (false, iced::window::Mode::Fullscreen),
        ] {
            let _ = update(
                &mut app,
                Message::WindowState {
                    size: Size::new(2560.0, 1440.0),
                    maximized: state.0,
                    mode: state.1,
                },
            );
        }
        assert_eq!(app.config.window_width, 1024.0);
        assert_eq!(app.config.window_height, 768.0);
    }

    #[test]
    fn only_a_normal_window_persists_its_size() {
        use iced::window::Mode;
        assert!(should_persist(false, Mode::Windowed));
        assert!(!should_persist(true, Mode::Windowed));
        assert!(!should_persist(false, Mode::Fullscreen));
    }

    #[test]
    fn resize_keeps_the_viewport_within_the_window() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::Resized(Size::new(1000.0, 800.0)));
        // Chrome (toolbar, footer, etc.) is subtracted, so the viewport
        // never exceeds the window and never collapses to zero.
        assert!(app.viewport_size.width > 0.0 && app.viewport_size.width <= 1000.0);
        assert!(app.viewport_size.height > 0.0 && app.viewport_size.height <= 800.0);
    }

    #[test]
    fn resize_refits_an_auto_zoomed_image() {
        use crate::app::state::DisplayedImage;
        use crate::app::test_support::{thumb, viewing_app};
        let mut app = viewing_app(&["a.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.displayed = DisplayedImage::Placeholder(thumb(2000, 1000));
            v.manual_zoom = false;
        }
        let _ = update(&mut app, Message::Resized(Size::new(800.0, 600.0)));
        // The 2000-wide image is shrunk to fit the smaller viewport.
        assert!(app.viewer().unwrap().zoom < 1.0);
    }

    #[test]
    fn close_requested_builds_a_save_then_close_task() {
        let mut app = empty_app();
        let _ = update(
            &mut app,
            Message::CloseRequested(iced::window::Id::unique()),
        );
    }
}
