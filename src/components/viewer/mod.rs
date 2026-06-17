mod view;

pub(crate) use view::{spinner, view};

#[derive(Debug, Clone)]
pub enum Message {
    Next,
    Prev,
    First,
    Last,
    NextRepeat,
    PrevRepeat,
    NextReleased,
    PrevReleased,
    ScrollZoom(f32),
    ZoomStep(i8),
    ZoomActual,
    ResetZoom,
    ToggleFullscreen,
    ToggleInfo,
    Rotate(u8),
    ToggleCheckerboard,
    ToggleHelp,
    Escape,
    DragStart,
    DragMove(iced::Point),
    CursorLeft,
    DragEnd,
}
use iced::Task;
use iced::time::Instant;

use crate::app::state::{Direction, DisplayedImage, DragState};
use crate::app::update::{NavTarget, fire_exif, fire_rotate, navigate, save_config};
use crate::app::viewer_math::{clamp_pan, compute_zoom, pan_for_zoom_toward_cursor};
use crate::app::{App, Message as AppMessage, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, recalc_viewport};

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Next => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Forward, Instant::now()));
            navigate(app, NavTarget::Delta(Direction::Forward))
        }
        Message::Prev => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Backward, Instant::now()));
            navigate(app, NavTarget::Delta(Direction::Backward))
        }
        Message::NextRepeat => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let past = viewer
                .held_direction
                .map(|(_, t)| t.elapsed() >= crate::app::HOLD_THRESHOLD)
                .unwrap_or(false);
            if !past {
                return Task::none();
            }
            navigate(app, NavTarget::Delta(Direction::Forward))
        }
        Message::PrevRepeat => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let past = viewer
                .held_direction
                .map(|(_, t)| t.elapsed() >= crate::app::HOLD_THRESHOLD)
                .unwrap_or(false);
            if !past {
                return Task::none();
            }
            navigate(app, NavTarget::Delta(Direction::Backward))
        }
        Message::First => navigate(app, NavTarget::Index(0)),
        Message::Last => {
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            navigate(app, NavTarget::Index(viewer.nav.len().saturating_sub(1)))
        }
        Message::ToggleFullscreen => {
            app.fullscreen = !app.fullscreen;
            recalc_viewport(app);
            let mode = if app.fullscreen {
                iced::window::Mode::Fullscreen
            } else {
                iced::window::Mode::Windowed
            };
            iced::window::latest().and_then(move |id| iced::window::set_mode(id, mode))
        }
        Message::Escape => {
            if app.modal.is_some() {
                app.modal = None;
                return Task::none();
            }
            if app.help_open {
                app.help_open = false;
                return Task::none();
            }
            if app.fullscreen {
                return update(app, Message::ToggleFullscreen);
            }
            app.open_menu = None;
            app.context_menu_pos = None;
            Task::none()
        }
        Message::NextReleased => {
            if let Some(viewer) = app.viewer_mut()
                && viewer
                    .held_direction
                    .map(|(d, _)| d == Direction::Forward)
                    .unwrap_or(false)
            {
                viewer.held_direction = None;
            }
            Task::none()
        }
        Message::PrevReleased => {
            if let Some(viewer) = app.viewer_mut()
                && viewer
                    .held_direction
                    .map(|(d, _)| d == Direction::Backward)
                    .unwrap_or(false)
            {
                viewer.held_direction = None;
            }
            Task::none()
        }
        Message::ScrollZoom(delta_y) => {
            let viewport = app.viewport_size;
            let cursor = app.last_cursor_pos;
            let toolbar_offset = if app.config.show_toolbar {
                crate::app::TOOLBAR_HEIGHT
            } else {
                0.0
            };
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let old = viewer.zoom;
            let factor = if delta_y > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            let new = (old * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            if (new - old).abs() < f32::EPSILON {
                return Task::none();
            }
            viewer.zoom = new;
            viewer.manual_zoom = true;
            let d = (
                cursor.x - viewport.width / 2.0,
                cursor.y - toolbar_offset - viewport.height / 2.0,
            );
            viewer.pan = pan_for_zoom_toward_cursor(viewer.pan, viewer.zoom / old, d);
            if let Some((w, h)) = viewer.displayed.original_size() {
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }
        Message::ZoomStep(direction) => {
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let old = viewer.zoom;
            let factor = if direction > 0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            let new = (old * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            if (new - old).abs() < f32::EPSILON {
                return Task::none();
            }
            viewer.zoom = new;
            viewer.manual_zoom = true;
            viewer.pan = pan_for_zoom_toward_cursor(viewer.pan, viewer.zoom / old, (0.0, 0.0));
            if let Some((w, h)) = viewer.displayed.original_size() {
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }
        Message::ZoomActual => {
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.zoom = 1.0;
            viewer.manual_zoom = true;
            if let Some((w, h)) = viewer.displayed.original_size() {
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }
        Message::ResetZoom => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.manual_zoom = false;
            viewer.pan = (0.0, 0.0);
            if let Some((w, h)) = viewer.displayed.original_size() {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }
            Task::none()
        }
        Message::DragStart => {
            let cursor = app.last_cursor_pos;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.drag = Some(DragState {
                start: cursor,
                start_pan: viewer.pan,
            });
            Task::none()
        }
        Message::DragMove(pos) => {
            app.last_cursor_pos = pos;
            let viewport = app.viewport_size;
            if let Some(viewer) = app.viewer_mut() {
                if viewer.video.is_some() {
                    viewer.video_controls_until =
                        Some(Instant::now() + crate::app::VIDEO_CONTROLS_TIMEOUT);
                }
                if let Some(ds) = viewer.drag {
                    let dx = pos.x - ds.start.x;
                    let dy = pos.y - ds.start.y;
                    let new_pan = (ds.start_pan.0 + dx, ds.start_pan.1 + dy);
                    if let Some((w, h)) = viewer.displayed.original_size() {
                        let img_w = w as f32 * viewer.zoom;
                        let img_h = h as f32 * viewer.zoom;
                        viewer.pan = clamp_pan(new_pan, img_w, img_h, viewport);
                    }
                }
            }
            Task::none()
        }
        Message::CursorLeft => {
            if let Some(viewer) = app.viewer_mut() {
                viewer.video_controls_until = None;
            }
            Task::none()
        }
        Message::DragEnd => {
            if let Some(viewer) = app.viewer_mut() {
                viewer.drag = None;
            }
            Task::none()
        }
        Message::Rotate(turns) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            if !matches!(viewer.displayed, DisplayedImage::Full { .. }) {
                return Task::none();
            }
            viewer.rotation = (viewer.rotation + turns) % 4;
            fire_rotate(viewer)
        }
        Message::ToggleCheckerboard => {
            app.config.show_checkerboard = !app.config.show_checkerboard;
            save_config(app)
        }
        Message::ToggleHelp => {
            app.help_open = !app.help_open;
            Task::none()
        }
        Message::ToggleInfo => {
            app.config.show_info = !app.config.show_info;
            recalc_viewport(app);
            let probe = if app.config.show_info {
                fire_exif(app)
            } else {
                Task::none()
            };
            Task::batch([save_config(app), probe])
        }
    }
}
