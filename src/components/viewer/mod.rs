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
use iced::Size;
use iced::Task;
use iced::time::Instant;

use crate::app::state::{Direction, DisplayedImage, DragState, Viewer};
use crate::app::update::window::Message as WindowMessage;
use crate::app::update::{
    NavTarget, complete_navigation, fire_exif, fire_rotate, navigate, push_toast, save_config,
    scrub_to,
};
use crate::app::viewer_math::{
    clamp_pan, compute_zoom, nudge_zoom_percent, pan_for_zoom_toward_cursor,
};
use crate::app::{
    Message as AppMessage, Shared, Window, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, recalc_viewport,
};
use crate::components::toasts::ToastKind;
use crate::config::ZoomMode;
use crate::media::store::ImageKey;

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    let fingerprint = |win: &Window| {
        win.viewer()
            .map(|v| (v.zoom, v.nav.cursor(), v.pan, win.viewport_size))
    };
    let before = fingerprint(win);
    let task = update_view(win, shared, message);
    // A pan streams a tiled still's missing tiles immediately. A zoom change,
    // navigation, or viewport change moves to a placement no frame has
    // stamped yet, so its demand instead waits for the view to rest, never
    // producing soon-obsolete tiles. An unchanged view fires nothing: every
    // bare cursor move arrives here as a DragMove, and a per-move demand
    // pass over a tiled still is pure lock and hash churn.
    let after = fingerprint(win);
    let tiles = match (before, after) {
        (_, None) => Task::none(),
        (Some(b), Some(a)) if b == a => Task::none(),
        (Some((zoom, cursor, _, viewport)), Some((z, c, _, vp)))
            if zoom == z && cursor == c && viewport == vp =>
        {
            // Only the pan moved.
            crate::app::update::fire_tiles(win, shared)
        }
        _ => crate::app::update::settle_tiles(win),
    };
    Task::batch([task, tiles])
}

fn update_view(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
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
            // The modal, zoom pop-up, help sheet, and open menus are dismissed
            // by the app dispatch policy (cross-cutting overlay state) before
            // this runs. Fullscreen, which the viewer owns, exits here.
            if win.fullscreen {
                return update(win, shared, Message::ToggleFullscreen);
            }
            Task::none()
        }
        Message::NextReleased => release_hold(win, shared, Direction::Forward),
        Message::PrevReleased => release_hold(win, shared, Direction::Backward),
        Message::ScrollZoom(delta_y) => {
            let viewport = win.viewport_size;
            let cursor = win.last_cursor_pos;
            // Fullscreen hides the toolbar, so no offset applies there.
            let toolbar_offset = if shared.config.standard.chrome.toolbar && !win.fullscreen {
                crate::app::TOOLBAR_HEIGHT
            } else {
                0.0
            };
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            let factor = if delta_y > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            let cursor_offset = (
                cursor.x - viewport.width / 2.0,
                cursor.y - toolbar_offset - viewport.height / 2.0,
            );
            let target = viewer.zoom * factor;
            if !viewer.apply_zoom(target, cursor_offset, viewport) {
                return Task::none();
            }
            // A wheel zoom reaches even an unfocused (hovered) window, where the
            // image may have decayed. Restore it to full-res (re-decoding if its
            // RAM was evicted) and restart its decay, debounced so a wheel spin
            // reactivates once.
            if needs_reactivation(win) {
                Task::future(async {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    AppMessage::Window(WindowMessage::Reactivate)
                })
            } else {
                Task::none()
            }
        }
        Message::ZoomStep(direction) => {
            let viewport = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            let factor = if direction > 0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            let target = viewer.zoom * factor;
            viewer.apply_zoom(target, (0.0, 0.0), viewport);
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
            let zoom_mode = shared.config.standard.display.zoom_mode;
            let viewport = win.viewport_size;
            if let Some(viewer) = win.viewer_mut() {
                viewer.manual_zoom = false;
                viewer.pan = (0.0, 0.0);
                viewer.refit(zoom_mode, viewport);
            }
            Task::none()
        }
        Message::SetZoom(zoom) => {
            let viewport = win.viewport_size;
            if let Some(viewer) = win.viewer_mut() {
                viewer.apply_zoom(zoom, (0.0, 0.0), viewport);
            }
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
            let viewport = win.viewport_size;
            if let Some(viewer) = win.viewer_mut() {
                let target = nudge_zoom_percent(viewer.zoom, dir, ZOOM_MIN, ZOOM_MAX);
                viewer.apply_zoom(target, (0.0, 0.0), viewport);
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
                if viewer.video.session.is_some() {
                    viewer.video.controls_until =
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
                viewer.video.controls_until = None;
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
            let Some(viewer) = win.viewer() else {
                return Task::none();
            };
            if !matches!(viewer.displayed, DisplayedImage::Full { .. }) {
                return Task::none();
            }
            // A substrate past the texture limit cannot re-upload rotated:
            // refuse rather than draw it distorted. The true size stands in
            // while the RAM is mid-re-decode.
            let max = crate::media::registry::MAX_TEXTURE_DIM;
            let too_large = viewer
                .displayed_path
                .as_ref()
                .map(|path| ImageKey::new(&viewer.source, path))
                .and_then(|key| shared.store.ram(&key))
                .map(|ram| match &ram.handle {
                    iced::widget::image::Handle::Rgba { width, height, .. } => {
                        width.max(height) > &max
                    }
                    _ => false,
                })
                .unwrap_or_else(|| {
                    viewer
                        .displayed
                        .original_size()
                        .is_some_and(|(w, h)| w.max(h) > max)
                });
            if too_large {
                return push_toast(
                    win,
                    shared,
                    ToastKind::Info,
                    "This image is too large to rotate.".into(),
                );
            }
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.rotation = (viewer.rotation + turns) % 4;
            fire_rotate(viewer, &shared.store)
        }
        Message::ToggleCheckerboard => {
            shared.config.standard.display.checkerboard =
                !shared.config.standard.display.checkerboard;
            save_config(win, shared)
        }
        Message::ToggleHelp => {
            win.help_open = !win.help_open;
            Task::none()
        }
        Message::ToggleInfo => {
            shared.config.standard.chrome.info = !shared.config.standard.chrome.info;
            recalc_viewport(win, shared);
            let probe = if shared.config.standard.chrome.info {
                fire_exif(win, shared)
            } else {
                Task::none()
            };
            Task::batch([save_config(win, shared), probe])
        }
    }
}

/// Whether a wheel zoom should reactivate the window: only an unfocused window
/// whose on-screen image has decayed (demoted to view-res, its texture dropped,
/// or its RAM evicted). A focused window keeps its image full-res, and a wheel
/// zoom is the one zoom path that reaches a window without focusing it.
fn needs_reactivation(win: &Window) -> bool {
    if win.focused {
        return false;
    }
    let Some(viewer) = win.viewer() else {
        return false;
    };
    let Some(path) = viewer.displayed_path.as_deref() else {
        return false;
    };
    // Reactivate unless this window already holds the on-screen image at a
    // full-res texture. A lower demand (decayed to view-res) or a missing texture
    // (dropped, or evicted with no lease at all) means a wheel zoom should restore
    // it. Reads the lease directly, so it never lags a deferred reconcile.
    match viewer.cache.get(path) {
        None => true,
        Some(lease) => lease.want() < crate::media::store::Tier::Full || lease.texture().is_none(),
    }
}

impl Viewer {
    /// Apply an absolute zoom, re-anchoring the pan so the source point under
    /// `cursor_offset` (offset from the viewport center, `(0.0, 0.0)` for the
    /// center) stays fixed, then clamping the pan into the viewport. Returns
    /// whether the zoom changed: an epsilon-equal target is left untouched.
    fn apply_zoom(&mut self, zoom: f32, cursor_offset: (f32, f32), viewport: Size) -> bool {
        let old = self.zoom;
        let new = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if (new - old).abs() < f32::EPSILON {
            return false;
        }
        self.zoom = new;
        self.manual_zoom = true;
        self.pan = pan_for_zoom_toward_cursor(self.pan, new / old, cursor_offset);
        if let Some((w, h)) = self.displayed.original_size() {
            let img_w = w as f32 * new;
            let img_h = h as f32 * new;
            self.pan = clamp_pan(self.pan, img_w, img_h, viewport);
        }
        true
    }

    /// Recompute the fit zoom for `zoom_mode` unless a manual zoom is set, then
    /// clamp the pan into the viewport. The refit a resize or reset runs.
    pub(crate) fn refit(&mut self, zoom_mode: ZoomMode, viewport: Size) {
        if let Some((w, h)) = self.displayed.original_size() {
            if !self.manual_zoom {
                self.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }
            let img_w = w as f32 * self.zoom;
            let img_h = h as f32 * self.zoom;
            self.pan = clamp_pan(self.pan, img_w, img_h, viewport);
        }
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
    fn fullscreen_scroll_zoom_ignores_the_hidden_toolbar() {
        // Fullscreen hides the toolbar even while it is configured on, so
        // the wheel must anchor exactly as it does with no toolbar at all.
        let cursor = iced::Point::new(100.0, 100.0);

        let mut fs = viewing_app(&["a.png"], 0);
        fs.shared.config.standard.chrome.toolbar = true;
        fs.window.fullscreen = true;
        fs.window.last_cursor_pos = cursor;
        fs.viewer_mut().unwrap().zoom = 1.0;
        let _ = update(&mut fs.window, &mut fs.shared, Message::ScrollZoom(1.0));

        let mut plain = viewing_app(&["a.png"], 0);
        plain.shared.config.standard.chrome.toolbar = false;
        plain.window.last_cursor_pos = cursor;
        plain.viewer_mut().unwrap().zoom = 1.0;
        let _ = update(
            &mut plain.window,
            &mut plain.shared,
            Message::ScrollZoom(1.0),
        );

        assert_eq!(viewer(&fs).pan, viewer(&plain).pan);
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

    fn show_cached(app: &mut TestApp, path: &str, gpu_full: bool, resident: bool) {
        use crate::media::pipeline::Source;
        use crate::media::store::{ImageKey, RamImage, Tier};
        let p = std::path::PathBuf::from(path);
        let key = ImageKey::new(&Source::Fs, &p);
        let tier = match (resident, gpu_full) {
            (true, true) => Tier::Full,
            (true, false) => Tier::View,
            (false, _) => Tier::InRam,
        };
        let (lease, _) = app
            .shared
            .store
            .request(key.clone(), p.clone(), Source::Fs, tier);
        app.shared.store.on_decoded(
            key.clone(),
            RamImage {
                handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]),
                original_size: (2, 2),
                decode_time: None,
            },
        );
        if resident {
            app.shared
                .store
                .on_minted(key, tier, crate::ui::image_surface::test_keepalive());
        }
        let v = app.viewer_mut().unwrap();
        v.displayed_path = Some(path.into());
        v.cache.insert(p, lease);
    }

    #[test]
    fn needs_reactivation_only_for_a_decayed_unfocused_image() {
        let mut app = viewing_app(&["a.png"], 0);
        show_cached(&mut app, "a.png", false, true);

        // A focused window keeps its image full-res, so nothing to reactivate.
        app.window.focused = true;
        assert!(!needs_reactivation(&app.window));

        // Unfocused with a view-res image: reactivate on a wheel zoom.
        app.window.focused = false;
        assert!(needs_reactivation(&app.window));

        // A full-res, resident image needs no reactivation while unfocused.
        show_cached(&mut app, "a.png", true, true);
        assert!(!needs_reactivation(&app.window));

        // A released texture (full-res but no keepalive) does.
        show_cached(&mut app, "a.png", true, false);
        assert!(needs_reactivation(&app.window));

        // An evicted source (no cache entry) does.
        app.viewer_mut()
            .unwrap()
            .cache
            .remove(std::path::Path::new("a.png"));
        assert!(needs_reactivation(&app.window));
    }

    #[test]
    fn cursor_leave_clears_the_controls_clock() {
        let mut app = viewing_app(&["a.png"], 0);
        app.viewer_mut().unwrap().video.controls_until =
            Some(Instant::now() + std::time::Duration::from_secs(5));
        let _ = update(&mut app.window, &mut app.shared, Message::CursorLeft);
        assert!(viewer(&app).video.controls_until.is_none());
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
    fn close_zoom_slider_closes_it() {
        let mut app = viewing_app(&["a.png"], 0);
        app.window.zoom_slider_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::CloseZoomSlider);
        assert!(!app.window.zoom_slider_open);
    }

    #[test]
    fn toggle_checkerboard_flips_config() {
        let mut app = empty_app();
        let before = app.shared.config.standard.display.checkerboard;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::ToggleCheckerboard,
        );
        assert_eq!(app.shared.config.standard.display.checkerboard, !before);
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
        let before = app.shared.config.standard.chrome.info;
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleInfo);
        assert_eq!(app.shared.config.standard.chrome.info, !before);
    }

    #[test]
    fn toggle_fullscreen_fills_the_window() {
        let mut app = empty_app();
        app.window.window_size = iced::Size::new(1000.0, 800.0);
        let _ = update(&mut app.window, &mut app.shared, Message::ToggleFullscreen);
        assert!(app.window.fullscreen);
        assert_eq!(app.window.viewport_size, app.window.window_size);
    }

    // The overlay-dismissal priority chain (modal, zoom slider, help, menus)
    // moved to the app dispatch policy; its tests live beside it in
    // `app::update`. Fullscreen exit stays viewer-local.
    #[test]
    fn escape_exits_fullscreen() {
        let mut app = empty_app();
        app.window.fullscreen = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Escape);
        assert!(!app.window.fullscreen);
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
