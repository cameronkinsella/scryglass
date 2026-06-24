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
    SetZoom(f32),
    ToggleZoomSlider,
    CloseZoomSlider,
    NudgeZoom(i32),
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
use crate::app::update::{
    NavTarget, complete_navigation, fire_exif, fire_rotate, navigate, save_config, scrub_to,
};
use crate::app::viewer_math::{
    clamp_pan, compute_zoom, nudge_zoom_percent, pan_for_zoom_toward_cursor,
};
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
        // A held key scrubs at the repeat rate: the cursor advances no matter
        // what's loaded, showing each frame's blur or a spinner. The sharp
        // image loads once the key is released.
        Message::NextRepeat => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let past = viewer
                .held_direction
                .map(|(_, t)| t.elapsed() >= crate::app::HOLD_THRESHOLD)
                .unwrap_or(false);
            let len = viewer.nav.len();
            if !past || len == 0 {
                return Task::none();
            }
            let next = (viewer.nav.cursor() + 1) % len;
            scrub_to(app, next, false)
        }
        Message::PrevRepeat => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let past = viewer
                .held_direction
                .map(|(_, t)| t.elapsed() >= crate::app::HOLD_THRESHOLD)
                .unwrap_or(false);
            let len = viewer.nav.len();
            if !past || len == 0 {
                return Task::none();
            }
            let prev = (viewer.nav.cursor() + len - 1) % len;
            scrub_to(app, prev, false)
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
            if app.zoom_slider_open {
                app.zoom_slider_open = false;
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
        Message::NextReleased => release_hold(app, Direction::Forward),
        Message::PrevReleased => release_hold(app, Direction::Backward),
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
        Message::SetZoom(zoom) => {
            apply_zoom(app, zoom);
            Task::none()
        }
        Message::ToggleZoomSlider => {
            if app.zoom_slider_open {
                app.zoom_slider_open = false;
            } else if app
                .viewer()
                .and_then(|v| v.displayed.original_size())
                .is_some()
            {
                app.zoom_slider_open = true;
            }
            Task::none()
        }
        Message::CloseZoomSlider => {
            app.zoom_slider_open = false;
            Task::none()
        }
        Message::NudgeZoom(dir) => {
            if let Some(zoom) = app.viewer().map(|v| v.zoom) {
                apply_zoom(app, nudge_zoom_percent(zoom, dir, ZOOM_MIN, ZOOM_MAX));
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

/// Set an absolute zoom factor, zooming toward the viewport center.
fn apply_zoom(app: &mut App, zoom: f32) {
    let viewport = app.viewport_size;
    let Some(viewer) = app.viewer_mut() else {
        return;
    };
    let old = viewer.zoom;
    let new = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
    if (new - old).abs() < f32::EPSILON {
        return;
    }
    viewer.zoom = new;
    viewer.manual_zoom = true;
    viewer.pan = pan_for_zoom_toward_cursor(viewer.pan, new / old, (0.0, 0.0));
    if let Some((w, h)) = viewer.displayed.original_size() {
        let img_w = w as f32 * new;
        let img_h = h as f32 * new;
        viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
    }
}

/// On releasing a held navigation key, load the frame the scrub landed on. A
/// quick tap (under the hold threshold) never scrubbed, so leave it be.
fn release_hold(app: &mut App, dir: Direction) -> Task<AppMessage> {
    let mut was_hold = false;
    if let Some(viewer) = app.viewer_mut()
        && viewer.held_direction.is_some_and(|(d, _)| d == dir)
    {
        was_hold = viewer
            .held_direction
            .is_some_and(|(_, t)| t.elapsed() >= crate::app::HOLD_THRESHOLD);
        viewer.held_direction = None;
    }
    if !was_hold {
        return Task::none();
    }
    match app.viewer().map(|v| v.nav.cursor()) {
        Some(cursor) => complete_navigation(app, cursor, true),
        None => Task::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Modal;
    use crate::app::state::Viewer;
    use crate::app::test_support::{empty_app, viewing_app};

    fn viewer(app: &App) -> &Viewer {
        app.viewer().unwrap()
    }

    #[test]
    fn zoom_actual_sets_full_size_and_marks_manual() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 0.5;
        let _ = update(&mut app, Message::ZoomActual);
        assert_eq!(viewer(&app).zoom, 1.0);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_step_scales_in_then_back_out() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 1.0;
        let _ = update(&mut app, Message::ZoomStep(1));
        assert!((viewer(&app).zoom - ZOOM_STEP).abs() < 1e-5);
        let _ = update(&mut app, Message::ZoomStep(-1));
        assert!((viewer(&app).zoom - 1.0).abs() < 1e-5);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_step_clamps_at_the_maximum() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = ZOOM_MAX;
        let _ = update(&mut app, Message::ZoomStep(1));
        assert_eq!(viewer(&app).zoom, ZOOM_MAX);
    }

    #[test]
    fn reset_zoom_clears_manual_and_recenters() {
        let mut app = viewing_app(&["a.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.manual_zoom = true;
            v.pan = (40.0, -20.0);
        }
        let _ = update(&mut app, Message::ResetZoom);
        assert!(!viewer(&app).manual_zoom);
        assert_eq!(viewer(&app).pan, (0.0, 0.0));
    }

    #[test]
    fn cursor_leave_clears_the_controls_clock() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().video_controls_until =
            Some(Instant::now() + std::time::Duration::from_secs(5));
        let _ = update(&mut app, Message::CursorLeft);
        assert!(viewer(&app).video_controls_until.is_none());
    }

    #[test]
    fn set_zoom_clamps_and_marks_manual() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app, Message::SetZoom(999.0));
        assert_eq!(viewer(&app).zoom, ZOOM_MAX);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn nudge_zoom_steps_a_single_percent() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 0.62;
        let _ = update(&mut app, Message::NudgeZoom(1));
        assert!((viewer(&app).zoom - 0.63).abs() < 1e-5);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_slider_opens_only_with_a_displayed_image() {
        let mut app = viewing_app(&["a.png"], 0);
        // Nothing displayed yet: toggling is a no-op.
        let _ = update(&mut app, Message::ToggleZoomSlider);
        assert!(!app.zoom_slider_open);

        app.viewer_mut().unwrap().displayed = DisplayedImage::Video {
            original_size: (100, 100),
        };
        let _ = update(&mut app, Message::ToggleZoomSlider);
        assert!(app.zoom_slider_open);
        let _ = update(&mut app, Message::ToggleZoomSlider);
        assert!(!app.zoom_slider_open);
    }

    #[test]
    fn escape_closes_the_zoom_slider() {
        let mut app = viewing_app(&["a.png"], 0);
        app.zoom_slider_open = true;
        let _ = update(&mut app, Message::Escape);
        assert!(!app.zoom_slider_open);
    }

    #[test]
    fn close_zoom_slider_closes_it() {
        let mut app = viewing_app(&["a.png"], 0);
        app.zoom_slider_open = true;
        let _ = update(&mut app, Message::CloseZoomSlider);
        assert!(!app.zoom_slider_open);
    }

    #[test]
    fn toggle_checkerboard_flips_config() {
        let mut app = empty_app();
        let before = app.config.show_checkerboard;
        let _ = update(&mut app, Message::ToggleCheckerboard);
        assert_eq!(app.config.show_checkerboard, !before);
    }

    #[test]
    fn toggle_help_opens_the_overlay() {
        let mut app = empty_app();
        assert!(!app.help_open);
        let _ = update(&mut app, Message::ToggleHelp);
        assert!(app.help_open);
    }

    #[test]
    fn toggle_info_flips_config() {
        let mut app = viewing_app(&["a.png"], 0);
        let before = app.config.show_info;
        let _ = update(&mut app, Message::ToggleInfo);
        assert_eq!(app.config.show_info, !before);
    }

    #[test]
    fn toggle_fullscreen_fills_the_window() {
        let mut app = empty_app();
        app.window_size = iced::Size::new(1000.0, 800.0);
        let _ = update(&mut app, Message::ToggleFullscreen);
        assert!(app.fullscreen);
        assert_eq!(app.viewport_size, app.window_size);
    }

    #[test]
    fn escape_closes_the_modal_before_anything_else() {
        let mut app = empty_app();
        app.modal = Some(Modal::Settings);
        app.help_open = true;
        let _ = update(&mut app, Message::Escape);
        assert!(app.modal.is_none());
        // Help is left for the next Escape.
        assert!(app.help_open);
    }

    #[test]
    fn escape_closes_help_when_no_modal_is_open() {
        let mut app = empty_app();
        app.help_open = true;
        let _ = update(&mut app, Message::Escape);
        assert!(!app.help_open);
    }

    #[test]
    fn escape_exits_fullscreen_after_modal_and_help() {
        let mut app = empty_app();
        app.fullscreen = true;
        let _ = update(&mut app, Message::Escape);
        assert!(!app.fullscreen);
    }

    #[test]
    fn escape_clears_menus_when_nothing_else_is_open() {
        let mut app = empty_app();
        app.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app, Message::Escape);
        assert!(app.context_menu_pos.is_none());
    }

    #[test]
    fn drag_start_then_end_tracks_drag_state() {
        let mut app = viewing_app(&["a.png"], 0);
        app.last_cursor_pos = iced::Point::new(10.0, 20.0);
        let _ = update(&mut app, Message::DragStart);
        assert!(viewer(&app).drag.is_some());
        let _ = update(&mut app, Message::DragEnd);
        assert!(viewer(&app).drag.is_none());
    }

    #[test]
    fn next_holds_the_direction_and_defers_on_a_cache_miss() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::Next);
        let v = viewer(&app);
        assert_eq!(v.held_direction.map(|(d, _)| d), Some(Direction::Forward));
        assert!(v.pending_nav.is_some());
    }

    #[test]
    fn next_released_clears_a_matching_hold() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::Next);
        let _ = update(&mut app, Message::NextReleased);
        assert!(viewer(&app).held_direction.is_none());
    }

    #[test]
    fn rotate_is_a_no_op_without_a_decoded_image() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app, Message::Rotate(1));
        assert_eq!(viewer(&app).rotation, 0);
    }

    #[test]
    fn a_held_repeat_scrubs_the_cursor_even_with_no_thumbnail() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        // A key held past the repeat threshold.
        let held =
            Instant::now() - crate::app::HOLD_THRESHOLD - std::time::Duration::from_millis(10);
        app.viewer_mut().unwrap().held_direction = Some((Direction::Forward, held));
        let _ = update(&mut app, Message::NextRepeat);
        // The cursor advances without waiting on b.png's blur.
        assert_eq!(viewer(&app).nav.cursor(), 1);
    }

    #[test]
    fn a_repeat_before_the_hold_threshold_does_not_move() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().held_direction = Some((Direction::Forward, Instant::now()));
        let _ = update(&mut app, Message::NextRepeat);
        assert_eq!(viewer(&app).nav.cursor(), 0);
    }
}
