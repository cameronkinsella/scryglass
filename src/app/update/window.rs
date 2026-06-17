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
