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
    EdgeEnter(Direction),
    EdgeExit,
    EdgePress(Direction),
    EdgeRepeat,
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
use crate::app::{
    MediaMessage, Message as AppMessage, Shared, Window, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
    recalc_viewport,
};

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Next => {
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Forward, Instant::now()));
            navigate(win, shared, NavTarget::Delta(Direction::Forward))
        }
        Message::Prev => {
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Backward, Instant::now()));
            navigate(win, shared, NavTarget::Delta(Direction::Backward))
        }
        // A held key scrubs at the repeat rate: the cursor advances no matter
        // what's loaded, showing each frame's blur or a spinner. The sharp
        // image loads once the key is released.
        Message::NextRepeat => {
            let Some(viewer) = win.viewer_mut() else {
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
            scrub_to(win, shared, next, false)
        }
        Message::PrevRepeat => {
            let Some(viewer) = win.viewer_mut() else {
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
            scrub_to(win, shared, prev, false)
        }
        Message::First => navigate(win, shared, NavTarget::Index(0)),
        Message::Last => {
            let Some(viewer) = win.viewer() else {
                return Task::none();
            };
            navigate(
                win,
                shared,
                NavTarget::Index(viewer.nav.len().saturating_sub(1)),
            )
        }
        Message::ToggleFullscreen => {
            win.fullscreen = !win.fullscreen;
            recalc_viewport(win, shared);
            if win.fullscreen {
                iced::window::set_mode(win.id, iced::window::Mode::Fullscreen)
            } else {
                // Fullscreen is independent of maximize: exiting returns to the
                // windowed mode, then re-maximizes if it was maximized beneath.
                let exit = iced::window::set_mode(win.id, iced::window::Mode::Windowed);
                if win.maximized {
                    exit.chain(iced::window::maximize(win.id, true))
                } else {
                    exit
                }
            }
        }
        Message::Escape => {
            if win.modal.is_some() {
                win.modal = None;
                return Task::none();
            }
            if win.zoom_slider_open {
                win.zoom_slider_open = false;
                return Task::none();
            }
            if win.help_open {
                win.help_open = false;
                return Task::none();
            }
            if win.fullscreen {
                return update(win, shared, Message::ToggleFullscreen);
            }
            win.open_menu = None;
            win.context_menu_pos = None;
            Task::none()
        }
        Message::NextReleased => release_hold(win, shared, Direction::Forward),
        Message::PrevReleased => release_hold(win, shared, Direction::Backward),
        Message::ScrollZoom(delta_y) => {
            let viewport = win.viewport_size;
            let cursor = win.last_cursor_pos;
            let toolbar_offset = if shared.config.show_toolbar {
                crate::app::TOOLBAR_HEIGHT
            } else {
                0.0
            };
            let Some(viewer) = win.viewer_mut() else {
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
            // A wheel zoom reaches even an unfocused (hovered) window, where the
            // image may have been demoted to view-res. Re-promote it to full-res
            // so it re-sharpens, debounced: PromoteCurrent is a no-op once full.
            match scroll_rederive_target(win) {
                Some(path) => Task::future(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    AppMessage::Media(MediaMessage::PromoteCurrent(path))
                }),
                None => Task::none(),
            }
        }
        Message::ZoomStep(direction) => {
            let viewport = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
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
            let viewport = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
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
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
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
            apply_zoom(win, shared, zoom);
            Task::none()
        }
        Message::ToggleZoomSlider => {
            if win.zoom_slider_open {
                win.zoom_slider_open = false;
            } else if win
                .viewer()
                .and_then(|v| v.displayed.original_size())
                .is_some()
            {
                win.zoom_slider_open = true;
            }
            Task::none()
        }
        Message::CloseZoomSlider => {
            win.zoom_slider_open = false;
            Task::none()
        }
        Message::NudgeZoom(dir) => {
            if let Some(zoom) = win.viewer().map(|v| v.zoom) {
                apply_zoom(
                    win,
                    shared,
                    nudge_zoom_percent(zoom, dir, ZOOM_MIN, ZOOM_MAX),
                );
            }
            Task::none()
        }
        Message::DragStart => {
            let cursor = win.last_cursor_pos;
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.drag = Some(DragState {
                start: cursor,
                start_pan: viewer.pan,
            });
            Task::none()
        }
        Message::DragMove(pos) => {
            win.last_cursor_pos = pos;
            let viewport = win.viewport_size;
            if let Some(viewer) = win.viewer_mut() {
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
            if let Some(viewer) = win.viewer_mut() {
                viewer.video_controls_until = None;
            }
            Task::none()
        }
        Message::DragEnd => {
            if let Some(viewer) = win.viewer_mut() {
                viewer.drag = None;
            }
            end_edge_hold(win, shared)
        }
        Message::EdgeEnter(dir) => {
            if let Some(viewer) = win.viewer_mut() {
                viewer.edge_hover = Some(dir);
            }
            Task::none()
        }
        // Leaving the strip ends the hold like a release, so it can't resume
        // when the cursor wanders back in. A fresh press is needed.
        Message::EdgeExit => {
            if let Some(viewer) = win.viewer_mut() {
                viewer.edge_hover = None;
            }
            end_edge_hold(win, shared)
        }
        // Reuse the keyboard step, with edge_held arming the repeat timer.
        Message::EdgePress(dir) => {
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.edge_held = Some(dir);
            let step = match dir {
                Direction::Forward => Message::Next,
                Direction::Backward => Message::Prev,
            };
            update(win, shared, step)
        }
        Message::EdgeRepeat => {
            let Some(dir) = win.viewer().and_then(|v| v.edge_held) else {
                return Task::none();
            };
            let repeat = match dir {
                Direction::Forward => Message::NextRepeat,
                Direction::Backward => Message::PrevRepeat,
            };
            update(win, shared, repeat)
        }
        Message::Rotate(turns) => {
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            if !matches!(viewer.displayed, DisplayedImage::Full { .. }) {
                return Task::none();
            }
            viewer.rotation = (viewer.rotation + turns) % 4;
            fire_rotate(viewer)
        }
        Message::ToggleCheckerboard => {
            shared.config.show_checkerboard = !shared.config.show_checkerboard;
            save_config(win, shared)
        }
        Message::ToggleHelp => {
            win.help_open = !win.help_open;
            Task::none()
        }
        Message::ToggleInfo => {
            shared.config.show_info = !shared.config.show_info;
            recalc_viewport(win, shared);
            let probe = if shared.config.show_info {
                fire_exif(win, shared)
            } else {
                Task::none()
            };
            Task::batch([save_config(win, shared), probe])
        }
    }
}

/// The on-screen image to re-promote to full-res after a wheel zoom, if any.
/// Only an unfocused window with a demoted (view-res) image needs it: a focused
/// window keeps its image full-res, and a wheel zoom is the one zoom path that
/// reaches a window without focusing it.
fn scroll_rederive_target(win: &Window) -> Option<std::path::PathBuf> {
    if win.focused {
        return None;
    }
    let viewer = win.viewer()?;
    let path = viewer.displayed_path.clone()?;
    if viewer.cache.peek(&path)?.gpu_full {
        return None;
    }
    Some(path)
}

/// Set an absolute zoom factor, zooming toward the viewport center.
fn apply_zoom(win: &mut Window, _shared: &mut Shared, zoom: f32) {
    let viewport = win.viewport_size;
    let Some(viewer) = win.viewer_mut() else {
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
fn release_hold(win: &mut Window, shared: &mut Shared, dir: Direction) -> Task<AppMessage> {
    let mut was_hold = false;
    if let Some(viewer) = win.viewer_mut()
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
    match win.viewer().map(|v| v.nav.cursor()) {
        Some(cursor) => complete_navigation(win, shared, cursor, true),
        None => Task::none(),
    }
}

/// End an active mouse edge-hold (the button came up or the cursor left the
/// strip), committing the frame it scrubbed to.
fn end_edge_hold(win: &mut Window, shared: &mut Shared) -> Task<AppMessage> {
    let Some(dir) = win.viewer().and_then(|v| v.edge_held) else {
        return Task::none();
    };
    if let Some(viewer) = win.viewer_mut() {
        viewer.edge_held = None;
    }
    release_hold(win, shared, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Modal;
    use crate::app::state::Viewer;
    use crate::app::test_support::{TestApp, empty_app, viewing_app};

    fn viewer(app: &TestApp) -> &Viewer {
        app.viewer().unwrap()
    }

    #[test]
    fn zoom_actual_sets_full_size_and_marks_manual() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 0.5;
        let _ = update(&mut app.window, &mut app.shared, Message::ZoomActual);
        assert_eq!(viewer(&app).zoom, 1.0);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_step_scales_in_then_back_out() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 1.0;
        let _ = update(&mut app.window, &mut app.shared, Message::ZoomStep(1));
        assert!((viewer(&app).zoom - ZOOM_STEP).abs() < 1e-5);
        let _ = update(&mut app.window, &mut app.shared, Message::ZoomStep(-1));
        assert!((viewer(&app).zoom - 1.0).abs() < 1e-5);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_step_clamps_at_the_maximum() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = ZOOM_MAX;
        let _ = update(&mut app.window, &mut app.shared, Message::ZoomStep(1));
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
        let _ = update(&mut app.window, &mut app.shared, Message::ResetZoom);
        assert!(!viewer(&app).manual_zoom);
        assert_eq!(viewer(&app).pan, (0.0, 0.0));
    }

    fn show_cached(app: &mut TestApp, path: &str, gpu_full: bool) {
        use crate::app::state::CachedImage;
        let image = CachedImage {
            handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]),
            original_size: (2, 2),
            keepalive: None,
            gpu_full,
        };
        let cost = image.byte_cost();
        let v = app.viewer_mut().unwrap();
        v.displayed_path = Some(path.into());
        v.cache.insert(path.into(), image, cost);
    }

    #[test]
    fn scroll_rederive_targets_only_a_demoted_unfocused_image() {
        let mut app = viewing_app(&["a.png"], 0);
        show_cached(&mut app, "a.png", false);

        // A focused window keeps its image full-res, so nothing to re-promote.
        app.window.focused = true;
        assert_eq!(scroll_rederive_target(&app.window), None);

        // Unfocused with a view-res image: re-promote it on a wheel zoom.
        app.window.focused = false;
        assert_eq!(
            scroll_rederive_target(&app.window),
            Some(std::path::PathBuf::from("a.png"))
        );

        // A full-res image needs no re-promote even while unfocused.
        show_cached(&mut app, "a.png", true);
        assert_eq!(scroll_rederive_target(&app.window), None);
    }

    #[test]
    fn cursor_leave_clears_the_controls_clock() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().video_controls_until =
            Some(Instant::now() + std::time::Duration::from_secs(5));
        let _ = update(&mut app.window, &mut app.shared, Message::CursorLeft);
        assert!(viewer(&app).video_controls_until.is_none());
    }

    #[test]
    fn set_zoom_clamps_and_marks_manual() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::SetZoom(999.0));
        assert_eq!(viewer(&app).zoom, ZOOM_MAX);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn nudge_zoom_steps_a_single_percent() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().zoom = 0.62;
        let _ = update(&mut app.window, &mut app.shared, Message::NudgeZoom(1));
        assert!((viewer(&app).zoom - 0.63).abs() < 1e-5);
        assert!(viewer(&app).manual_zoom);
    }

    #[test]
    fn zoom_slider_opens_only_with_a_displayed_image() {
        let mut app = viewing_app(&["a.png"], 0);
        // Nothing displayed yet: toggling is a no-op.
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleZoomSlider);
        assert!(!app.window.zoom_slider_open);

        app.viewer_mut().unwrap().displayed = DisplayedImage::Video {
            original_size: (100, 100),
        };
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleZoomSlider);
        assert!(app.window.zoom_slider_open);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleZoomSlider);
        assert!(!app.window.zoom_slider_open);
    }

    #[test]
    fn escape_closes_the_zoom_slider() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.zoom_slider_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(!app.window.zoom_slider_open);
    }

    #[test]
    fn close_zoom_slider_closes_it() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.zoom_slider_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::CloseZoomSlider);
        assert!(!app.window.zoom_slider_open);
    }

    #[test]
    fn toggle_checkerboard_flips_config() {
        let mut app = empty_app();
        let before = app.shared.config.show_checkerboard;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ToggleCheckerboard,
        );
        assert_eq!(app.shared.config.show_checkerboard, !before);
    }

    #[test]
    fn toggle_help_opens_the_overlay() {
        let mut app = empty_app();
        assert!(!app.window.help_open);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleHelp);
        assert!(app.window.help_open);
    }

    #[test]
    fn toggle_info_flips_config() {
        let mut app = viewing_app(&["a.png"], 0);
        let before = app.shared.config.show_info;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleInfo);
        assert_eq!(app.shared.config.show_info, !before);
    }

    #[test]
    fn toggle_fullscreen_fills_the_window() {
        let mut app = empty_app();
        app.window.window_size = iced::Size::new(1000.0, 800.0);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFullscreen);
        assert!(app.window.fullscreen);
        assert_eq!(app.window.viewport_size, app.window.window_size);
    }

    #[test]
    fn escape_closes_the_modal_before_anything_else() {
        let mut app = empty_app();
        app.window.modal = Some(Modal::Settings);
        app.window.help_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(app.window.modal.is_none());
        // Help is left for the next Escape.
        assert!(app.window.help_open);
    }

    #[test]
    fn escape_closes_help_when_no_modal_is_open() {
        let mut app = empty_app();
        app.window.help_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(!app.window.help_open);
    }

    #[test]
    fn escape_exits_fullscreen_after_modal_and_help() {
        let mut app = empty_app();
        app.window.fullscreen = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(!app.window.fullscreen);
    }

    #[test]
    fn escape_clears_menus_when_nothing_else_is_open() {
        let mut app = empty_app();
        app.window.context_menu_pos = Some(iced::Point::ORIGIN);
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(app.window.context_menu_pos.is_none());
    }

    #[test]
    fn drag_start_then_end_tracks_drag_state() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.last_cursor_pos = iced::Point::new(10.0, 20.0);
        let _ = update(&mut app.window, &mut app.shared, Message::DragStart);
        assert!(viewer(&app).drag.is_some());
        let _ = update(&mut app.window, &mut app.shared, Message::DragEnd);
        assert!(viewer(&app).drag.is_none());
    }

    #[test]
    fn next_holds_the_direction_and_defers_on_a_cache_miss() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::Next);
        let v = viewer(&app);
        assert_eq!(v.held_direction.map(|(d, _)| d), Some(Direction::Forward));
        assert!(v.pending_nav.is_some());
    }

    #[test]
    fn next_released_clears_a_matching_hold() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::Next);
        let _ = update(&mut app.window, &mut app.shared, Message::NextReleased);
        assert!(viewer(&app).held_direction.is_none());
    }

    #[test]
    fn edge_enter_and_exit_track_the_hovered_side() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::EdgeEnter(Direction::Backward),
        );
        assert_eq!(viewer(&app).edge_hover, Some(Direction::Backward));
        let _ = update(&mut app.window, &mut app.shared, Message::EdgeExit);
        assert!(viewer(&app).edge_hover.is_none());
    }

    #[test]
    fn edge_press_steps_once_and_arms_the_hold() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::EdgePress(Direction::Forward),
        );
        let v = viewer(&app);
        assert_eq!(v.edge_held, Some(Direction::Forward));
        assert_eq!(v.held_direction.map(|(d, _)| d), Some(Direction::Forward));
    }

    #[test]
    fn edge_repeat_scrubs_a_held_press() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let held =
            Instant::now() - crate::app::HOLD_THRESHOLD - std::time::Duration::from_millis(10);
        {
            let v = app.viewer_mut().unwrap();
            v.edge_held = Some(Direction::Forward);
            v.held_direction = Some((Direction::Forward, held));
        }
        let _ = update(&mut app.window, &mut app.shared, Message::EdgeRepeat);
        assert_eq!(viewer(&app).nav.cursor(), 1);
    }

    #[test]
    fn leaving_the_strip_cancels_the_hold() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let held =
            Instant::now() - crate::app::HOLD_THRESHOLD - std::time::Duration::from_millis(10);
        {
            let v = app.viewer_mut().unwrap();
            v.edge_held = Some(Direction::Forward);
            v.edge_hover = Some(Direction::Forward);
            v.held_direction = Some((Direction::Forward, held));
        }
        let _ = update(&mut app.window, &mut app.shared, Message::EdgeExit);
        assert!(viewer(&app).edge_held.is_none());
        // A later repeat tick can't resume it without a fresh press.
        let cursor = viewer(&app).nav.cursor();
        let _ = update(&mut app.window, &mut app.shared, Message::EdgeRepeat);
        assert_eq!(viewer(&app).nav.cursor(), cursor);
    }

    #[test]
    fn drag_end_clears_an_active_edge_hold() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().edge_held = Some(Direction::Forward);
        let _ = update(&mut app.window, &mut app.shared, Message::DragEnd);
        assert!(viewer(&app).edge_held.is_none());
    }

    #[test]
    fn rotate_is_a_no_op_without_a_decoded_image() {
        let mut app = viewing_app(&["a.png"], 0);
        let _ = update(&mut app.window, &mut app.shared, Message::Rotate(1));
        assert_eq!(viewer(&app).rotation, 0);
    }

    #[test]
    fn a_held_repeat_scrubs_the_cursor_even_with_no_thumbnail() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        // A key held past the repeat threshold.
        let held =
            Instant::now() - crate::app::HOLD_THRESHOLD - std::time::Duration::from_millis(10);
        app.viewer_mut().unwrap().held_direction = Some((Direction::Forward, held));
        let _ = update(&mut app.window, &mut app.shared, Message::NextRepeat);
        // The cursor advances without waiting on b.png's blur.
        assert_eq!(viewer(&app).nav.cursor(), 1);
    }

    #[test]
    fn a_repeat_before_the_hold_threshold_does_not_move() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().held_direction = Some((Direction::Forward, Instant::now()));
        let _ = update(&mut app.window, &mut app.shared, Message::NextRepeat);
        assert_eq!(viewer(&app).nav.cursor(), 0);
    }
}
