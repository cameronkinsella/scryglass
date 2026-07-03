//! Per-window OS events: geometry tracking and persistence, focus, and the
//! minimize poll. The decay engine these events drive lives in `decay`.

#[derive(Debug, Clone)]
pub enum Message {
    Resized(Size),
    Moved(iced::Point),
    /// Maximized/minimized/mode/snapped fetched after a resize or move, the
    /// gate for persisting the live geometry only while plainly windowed.
    WindowState {
        maximized: bool,
        minimized: bool,
        mode: iced::window::Mode,
        snapped: bool,
    },
    /// The settle timer after a move or resize fired. A later event bumps the
    /// generation, so a mid-drag probe no-ops.
    ProbeWindowState {
        generation: u64,
    },
    /// Native focus gained or lost, tracked per window for the resource states.
    Focused(bool),
    /// A decay stage firing for the carried decay generation. A later focus
    /// or minimize change bumps the generation, so a superseded timer no-ops.
    Decay {
        generation: u64,
        stage: DecayStage,
    },
    /// Re-checks OS-minimize state, off the focus-change events and a slow
    /// unfocused fallback, since iced has no minimize event.
    CheckMinimize,
    /// Result of the minimize poll.
    Minimized(bool),
    /// A scroll reached an unfocused window: restore its on-screen image to
    /// full-res (re-decoding if evicted) and restart its decay, so a zoom there
    /// is crisp without bringing the window forward.
    Reactivate,
    CloseRequested(iced::window::Id),
    /// iced's laid-out size of this window's image area (`area`), measured after
    /// layout for the window size `at`. Corrects the chrome-estimated viewport to the
    /// true area, so the fit zoom and view-res bake match what is on screen and the
    /// demote stays seamless. `at` is carried so a measurement the window has already
    /// resized past is dropped rather than applied to a stale layout.
    ImageAreaMeasured {
        area: Size,
        at: Size,
    },
}

use std::time::Duration;

use iced::{Size, Task};

use super::decay::{self, DecayStage};
use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{Message as AppMessage, Shared, Window, recalc_viewport};

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Resized(size) => {
            // A Windows minimize reports a zero-size resize. Acting on it would
            // clamp the pan and shift the filmstrip against a viewport that
            // does not exist, and the restore never undoes either. The window
            // keeps its real size and the restore's resize carries on from it.
            if size.width <= 0.0 || size.height <= 0.0 {
                return Task::none();
            }
            win.window_size = size;
            recalc_viewport(win, shared);
            let zoom_mode = shared.config.zoom_mode;
            let viewport = win.viewport_size;

            if let Some(viewer) = win.viewer_mut()
                && let Some((w, h)) = viewer.displayed.original_size()
            {
                if !viewer.manual_zoom {
                    viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
                }
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }

            // A new width would reveal strip only on the right (its offset is
            // measured from the left). Shift by half the delta so it grows
            // evenly. Skipped in fullscreen, where the strip is hidden, so the
            // round trip back lands where it started.
            let mut strip = Task::none();
            if !win.fullscreen {
                let id = win.id;
                if let Some(viewer) = win.viewer_mut() {
                    if viewer.filmstrip_width != 0.0 && viewer.filmstrip_width != size.width {
                        let offset = crate::components::filmstrip::resized_offset(
                            viewer.filmstrip_scroll_x,
                            viewer.filmstrip_width,
                            size.width,
                            viewer.nav.len(),
                        );
                        viewer.filmstrip_scroll_x = offset;
                        strip = iced::widget::operation::scroll_to(
                            crate::components::filmstrip::filmstrip_id(id),
                            iced::widget::scrollable::AbsoluteOffset { x: offset, y: 0.0 },
                        );
                    }
                    viewer.filmstrip_width = size.width;
                }
            }

            // A resize changes the placement like a zoom, so its demand is
            // debounced too: it must run after a frame draws (and stamps)
            // the new geometry, and a live resize storm coalesces to one.
            let tiles = super::settle_tiles(win);
            // The app's own fullscreen never persists. A natively maximized or
            // fullscreened window looks like any other resize here, so confirm
            // the state before persisting and let the windowed size stand.
            if win.fullscreen {
                Task::batch([tiles, strip])
            } else {
                Task::batch([tiles, strip, debounce_window_state(win)])
            }
        }

        Message::Moved(pos) => {
            win.window_pos = pos;
            // Persist through the same state query as a resize, never directly:
            // a maximize repositions the window and fires Moved before its
            // maximized state is known, which would save those bounds as the
            // restored position.
            if win.fullscreen {
                Task::none()
            } else {
                debounce_window_state(win)
            }
        }

        Message::ProbeWindowState { generation } => {
            if generation != win.probe_generation {
                return Task::none();
            }
            check_window_state(win.id)
        }

        Message::WindowState {
            maximized,
            minimized,
            mode,
            snapped,
        } => {
            // A minimized window reports it is neither maximized nor at its real
            // bounds. Leave both untouched so the restore stack ignores minimize
            // and the next window reopens in the pre-minimize state. The config
            // is written only on close, so it tracks the last closed window.
            if !minimized {
                win.maximized = maximized;
                if should_persist(maximized, minimized, mode, snapped) {
                    win.restored_size = win.window_size;
                    win.restored_pos = win.window_pos;
                }
            }
            Task::none()
        }

        Message::Focused(focused) => {
            let changed = win.focused != focused;
            win.focused = focused;
            // Losing focus dismisses the zoom pop-up, which has no owner to track.
            if !focused {
                win.zoom_slider_open = false;
            }
            if !changed {
                return Task::none();
            }
            // A focus change brackets a minimize/restore, so confirm the minimize
            // state on it instead of polling, and restart the decay pipeline.
            Task::batch([decay::restart_decay(win, shared), check_minimize(win.id)])
        }

        Message::Decay { generation, stage } => {
            decay::run_decay_stage(win, shared, generation, stage)
        }

        Message::Reactivate => decay::restart_decay(win, shared),

        Message::CheckMinimize => check_minimize(win.id),

        Message::Minimized(minimized) => {
            let changed = win.minimized != minimized;
            win.minimized = minimized;
            if !changed {
                return Task::none();
            }
            // Pause an open video the instant the window minimizes (audio stops at
            // once, not after the frame queue stalls), and resume on restore only
            // if it was playing. The pause is opt-out via config. An unfocused but
            // un-minimized video keeps playing regardless.
            let pause = shared.config.resource.minimized.pause_video;
            let mut resume = win.video_resumes_on_restore;
            if let Some(session) = win.viewer_mut().and_then(|v| v.video.session.as_mut()) {
                if minimized && pause {
                    resume = session.playing;
                    session.pause();
                } else if !minimized && resume {
                    session.play();
                    resume = false;
                }
            }
            win.video_resumes_on_restore = resume;
            decay::restart_decay(win, shared)
        }

        Message::ImageAreaMeasured { area: size, at } => {
            // Drop a measurement the window has since resized past: it was taken for a
            // stale layout, so applying it (or calibrating from it) would fight the
            // live resize. A fresh one follows for the settled size.
            if at != win.window_size {
                return Task::none();
            }
            // Correct the chrome-estimated viewport to iced's true image area, so the
            // fit zoom and the view-res bake match what is on screen and the demote
            // stays seamless. Ignore a measurement that already agrees, so this does
            // not re-fit every frame.
            if (size.width - win.viewport_size.width).abs() < 0.5
                && (size.height - win.viewport_size.height).abs() < 0.5
            {
                return Task::none();
            }
            // Calibrate the chrome estimate to iced's true layout, so a resize tracks
            // the real area synchronously and this async measurement stops fighting
            // the estimate frame to frame. Skip in fullscreen, where the image owns
            // the whole window and there is no chrome to learn.
            if !win.fullscreen {
                win.chrome_pad.width += win.viewport_size.width - size.width;
                win.chrome_pad.height += win.viewport_size.height - size.height;
            }
            win.viewport_size = size;
            let zoom_mode = shared.config.zoom_mode;
            if let Some(viewer) = win.viewer_mut()
                && let Some((w, h)) = viewer.displayed.original_size()
            {
                if !viewer.manual_zoom {
                    viewer.zoom = compute_zoom(zoom_mode, w, h, size);
                }
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, size);
            }
            // The corrected viewport shifts the placement, like a resize.
            super::settle_tiles(win)
        }

        Message::CloseRequested(id) => {
            // Persist this window's full restore stack as the next window's
            // geometry: the restored windowed bounds, plus the maximized and
            // fullscreen flags to replay on top.
            shared.config.window_width = win.restored_size.width;
            shared.config.window_height = win.restored_size.height;
            shared.config.window_x = Some(win.restored_pos.x);
            shared.config.window_y = Some(win.restored_pos.y);
            shared.config.window_maximized = win.maximized;
            shared.config.window_fullscreen = win.fullscreen;
            let config = shared.config.clone();
            Task::future(config.save()).then(move |_| iced::window::close(id))
        }
    }
}

/// How long a window must sit still before its state is probed for the
/// geometry save. A drag streams events. Probing only after they settle keeps
/// mid-drag positions (the instant before a snap layout applies) out of the
/// saved geometry.
const PROBE_SETTLE: Duration = Duration::from_millis(400);

/// Arm the settle timer for a state probe, superseding any pending one.
fn debounce_window_state(win: &mut Window) -> Task<AppMessage> {
    win.probe_generation = win.probe_generation.wrapping_add(1);
    let generation = win.probe_generation;
    Task::future(async move {
        tokio::time::sleep(PROBE_SETTLE).await;
        AppMessage::Window(Message::ProbeWindowState { generation })
    })
}

/// Ask the windowing system for the window's maximized, minimized, and mode
/// state, so the size is persisted only when it is the plain windowed size.
fn check_window_state(id: iced::window::Id) -> Task<AppMessage> {
    iced::window::is_maximized(id).then(move |maximized| {
        iced::window::mode(id).then(move |mode| {
            iced::window::is_minimized(id).then(move |minimized| {
                iced::window::raw_id::<AppMessage>(id).map(move |raw| {
                    AppMessage::Window(Message::WindowState {
                        maximized,
                        minimized: minimized.unwrap_or(false),
                        mode,
                        snapped: crate::platform::window_is_snapped(raw),
                    })
                })
            })
        })
    })
}

/// A size worth remembering only when the window is plainly windowed: neither
/// maximized, minimized, fullscreen, nor snapped. A snap layout is the OS
/// placing the window, so reopening there would lose the size the user chose.
fn should_persist(
    maximized: bool,
    minimized: bool,
    mode: iced::window::Mode,
    snapped: bool,
) -> bool {
    !maximized && !minimized && !snapped && mode == iced::window::Mode::Windowed
}

/// Query the OS minimize state. iced drops winit's `Occluded` and surfaces no
/// minimize event, so polling `is_minimized` on the focus changes that bracket
/// a minimize (plus a slow unfocused fallback) is deliberate, not a stopgap.
/// The only event-driven alternative is a Windows-only 0x0 `Resized`, and a
/// one-platform event cannot be regression-tested in CI the way a poll can.
/// Wayland alone cannot know, and so never reports, its own minimize state.
fn check_minimize(id: iced::window::Id) -> Task<AppMessage> {
    iced::window::is_minimized(id)
        .map(|m| AppMessage::Window(Message::Minimized(m.unwrap_or(false))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn resize_updates_the_window_size() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resized(Size::new(1024.0, 768.0)),
        );
        assert_eq!(app.window.window_size, Size::new(1024.0, 768.0));
    }

    #[test]
    fn the_zero_size_resize_of_a_minimize_is_ignored() {
        let mut app = crate::app::test_support::viewing_app(&["a.png", "b.png"], 0);
        {
            let v = app.viewer_mut().unwrap();
            v.filmstrip_scroll_x = 120.0;
            v.filmstrip_width = 800.0;
            v.pan = (40.0, 10.0);
            v.manual_zoom = true;
        }
        let before = app.window.window_size;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resized(Size::ZERO),
        );
        assert_eq!(app.window.window_size, before);
        let v = app.viewer().unwrap();
        assert_eq!(v.filmstrip_scroll_x, 120.0);
        assert_eq!(v.filmstrip_width, 800.0);
        assert_eq!(v.pan, (40.0, 10.0));
    }

    #[test]
    fn focus_change_is_tracked_and_dismisses_the_zoom_popup() {
        let mut app = empty_app();
        app.window.zoom_slider_open = true;
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        assert!(!app.window.focused);
        assert!(!app.window.zoom_slider_open);
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(true));
        assert!(app.window.focused);
    }

    #[test]
    fn minimize_poll_result_updates_the_flag() {
        let mut app = empty_app();
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        assert!(app.window.minimized);
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(false));
        assert!(!app.window.minimized);
    }

    #[test]
    fn the_windowed_gate_saves_the_live_geometry() {
        let mut app = empty_app();
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(120.0, 80.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert_eq!(app.window.restored_size, Size::new(1024.0, 768.0));
        assert_eq!(app.window.restored_pos, iced::Point::new(120.0, 80.0));
    }

    #[test]
    fn a_snapped_gate_keeps_the_windowed_geometry() {
        let mut app = empty_app();
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(120.0, 80.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        // A snap layout moves and resizes the window, but the saved geometry
        // must stay what the user chose, so a relaunch opens pre-snap.
        app.window.window_size = Size::new(1280.0, 1400.0);
        app.window.window_pos = iced::Point::new(1280.0, 0.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: true,
            },
        );
        assert_eq!(app.window.restored_size, Size::new(1024.0, 768.0));
        assert_eq!(app.window.restored_pos, iced::Point::new(120.0, 80.0));
    }

    #[test]
    fn a_maximized_gate_keeps_the_windowed_geometry() {
        let mut app = empty_app();
        // Establish the restored geometry while windowed.
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(120.0, 80.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        // Maximizing moves and resizes the window to its bounds, but the gate
        // must not overwrite the restored geometry (the position-loss bug fix).
        app.window.window_size = Size::new(2560.0, 1440.0);
        app.window.window_pos = iced::Point::new(0.0, 0.0);
        for state in [
            (true, iced::window::Mode::Windowed),
            (false, iced::window::Mode::Fullscreen),
        ] {
            let _ = update(
                &mut app.window,
                &mut app.shared,
                Message::WindowState {
                    maximized: state.0,
                    minimized: false,
                    mode: state.1,
                    snapped: false,
                },
            );
        }
        assert_eq!(app.window.restored_size, Size::new(1024.0, 768.0));
        assert_eq!(app.window.restored_pos, iced::Point::new(120.0, 80.0));
    }

    #[test]
    fn moves_supersede_the_pending_state_probe() {
        let mut app = empty_app();
        let before = app.window.probe_generation;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Moved(iced::Point::new(10.0, 10.0)),
        );
        let mid = app.window.probe_generation;
        assert_ne!(before, mid);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resized(Size::new(800.0, 600.0)),
        );
        assert_ne!(mid, app.window.probe_generation);
    }

    #[test]
    fn only_a_normal_window_persists_its_size() {
        use iced::window::Mode;
        assert!(should_persist(false, false, Mode::Windowed, false));
        assert!(!should_persist(true, false, Mode::Windowed, false));
        assert!(!should_persist(false, true, Mode::Windowed, false));
        assert!(!should_persist(false, false, Mode::Fullscreen, false));
        assert!(!should_persist(false, false, Mode::Windowed, true));
    }

    #[test]
    fn a_minimized_window_keeps_its_restored_geometry() {
        let mut app = empty_app();
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(120.0, 80.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        // Minimizing reports bogus off-screen bounds, which must not be saved.
        app.window.window_size = Size::new(0.0, 0.0);
        app.window.window_pos = iced::Point::new(-32000.0, -32000.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: true,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert_eq!(app.window.restored_size, Size::new(1024.0, 768.0));
        assert_eq!(app.window.restored_pos, iced::Point::new(120.0, 80.0));
    }

    #[test]
    fn a_minimized_window_keeps_its_maximized_flag() {
        let mut app = empty_app();
        // Maximize, then minimize (which reports not-maximized): the flag holds,
        // so the restore stack reopens maximized.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: true,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert!(app.window.maximized);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: true,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert!(app.window.maximized);
    }

    #[test]
    fn a_move_updates_the_tracked_position() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Moved(iced::Point::new(300.0, 200.0)),
        );
        assert_eq!(app.window.window_pos, iced::Point::new(300.0, 200.0));
    }

    #[test]
    fn window_state_records_the_maximized_flag() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: true,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert!(app.window.maximized);
    }

    #[test]
    fn close_persists_this_windows_restore_stack() {
        let mut app = empty_app();
        // The window's restored bounds and flags are what a close persists.
        app.window.restored_size = Size::new(1024.0, 768.0);
        app.window.restored_pos = iced::Point::new(120.0, 80.0);
        app.window.maximized = true;
        app.window.fullscreen = true;
        let id = app.window.id;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::CloseRequested(id),
        );
        assert_eq!(app.shared.config.window_width, 1024.0);
        assert_eq!(app.shared.config.window_height, 768.0);
        assert_eq!(app.shared.config.window_x, Some(120.0));
        assert_eq!(app.shared.config.window_y, Some(80.0));
        assert!(app.shared.config.window_maximized);
        assert!(app.shared.config.window_fullscreen);
    }

    #[test]
    fn an_open_windows_move_does_not_change_the_saved_geometry() {
        let mut app = empty_app();
        app.shared.config.window_x = Some(10.0);
        // An open window moving while another window's geometry is saved must
        // not overwrite it. Only a close persists.
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(500.0, 400.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
                snapped: false,
            },
        );
        assert_eq!(app.shared.config.window_x, Some(10.0));
    }

    #[test]
    fn resize_keeps_the_viewport_within_the_window() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resized(Size::new(1000.0, 800.0)),
        );
        // Chrome (toolbar, footer, etc.) is subtracted, so the viewport
        // never exceeds the window and never collapses to zero.
        assert!(app.window.viewport_size.width > 0.0 && app.window.viewport_size.width <= 1000.0);
        assert!(app.window.viewport_size.height > 0.0 && app.window.viewport_size.height <= 800.0);
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
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Resized(Size::new(800.0, 600.0)),
        );
        // The 2000-wide image is shrunk to fit the smaller viewport.
        assert!(app.viewer().unwrap().zoom < 1.0);
    }

    #[test]
    fn close_requested_builds_a_save_then_close_task() {
        let mut app = empty_app();
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::CloseRequested(iced::window::Id::unique()),
        );
    }
}
