use iced::Size;

#[derive(Debug, Clone)]
pub enum Message {
    Resized(Size),
    CloseRequested(iced::window::Id),
}
use iced::Task;

use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{App, Message as AppMessage, recalc_viewport};

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Resized(size) => {
            app.window_size = size;
            app.config.window_width = size.width;
            app.config.window_height = size.height;
            recalc_viewport(app);
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            if let Some((w, h)) = viewer.displayed.original_size() {
                if !viewer.manual_zoom {
                    viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
                }
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        Message::CloseRequested(id) => {
            let config = app.config.clone();
            Task::future(config.save()).then(move |_| iced::window::close(id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn resize_updates_window_and_config_dimensions() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::Resized(Size::new(1024.0, 768.0)));
        assert_eq!(app.window_size, Size::new(1024.0, 768.0));
        assert_eq!(app.config.window_width, 1024.0);
        assert_eq!(app.config.window_height, 768.0);
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
}
