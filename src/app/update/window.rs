use iced::Size;

#[derive(Debug, Clone)]
pub enum Message {
    Resized(Size),
    Moved(iced::Point),
    /// Maximized/minimized/mode fetched after a resize or move, the gate for
    /// persisting the live geometry only while plainly windowed.
    WindowState {
        maximized: bool,
        minimized: bool,
        mode: iced::window::Mode,
    },
    /// Native focus gained or lost, tracked per window for the resource tiers.
    Focused(bool),
    /// Fires after a window has been unfocused for [`PREFETCH_IDLE`]; drops the
    /// prefetch look-ahead if the carried tier generation is still current (no
    /// focus or minimize change has happened since it was armed).
    PrefetchIdle(u64),
    /// Re-checks OS-minimize state, off the focus-change events and a slow
    /// unfocused fallback, since iced has no minimize event.
    CheckMinimize,
    /// Result of the minimize poll.
    Minimized(bool),
    CloseRequested(iced::window::Id),
}

/// How long a window sits unfocused before its prefetch look-ahead is shed. A
/// brief glance away keeps it; only a sustained switch reclaims the neighbors.
const PREFETCH_IDLE: std::time::Duration = std::time::Duration::from_secs(15);
use iced::Task;

use super::{fire_prefetch, fire_reupload_res};
use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{Message as AppMessage, Shared, Window, recalc_viewport};

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Resized(size) => {
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

            // The app's own fullscreen never persists. A natively maximized or
            // fullscreened window looks like any other resize here, so confirm
            // the state before persisting and let the windowed size stand.
            if win.fullscreen {
                Task::none()
            } else {
                check_window_state(win.id)
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
                check_window_state(win.id)
            }
        }

        Message::WindowState {
            maximized,
            minimized,
            mode,
        } => {
            // A minimized window reports it is neither maximized nor at its real
            // bounds. Leave both untouched so the restore stack ignores minimize
            // and the next window reopens in the pre-minimize state. The config
            // is written only on close, so it tracks the last closed window.
            if !minimized {
                win.maximized = maximized;
                if should_persist(maximized, minimized, mode) {
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
            // A focus change brackets a minimize/restore, so confirm the
            // minimize state on it instead of polling continuously.
            let minimize = check_minimize(win.id);
            if focused {
                // Re-warm the look-ahead the idle drop may have shed, and
                // promote the on-screen image back to full-res for crisp zoom.
                // Bumping the generation supersedes any pending idle-drop timer.
                win.tier_generation = win.tier_generation.wrapping_add(1);
                let pipeline = shared.pipeline.clone();
                let depth = shared.config.prefetch_depth;
                let view = win.viewport_size;
                if let Some(viewer) = win.viewer_mut() {
                    let mut tasks = fire_prefetch(&pipeline, viewer, depth, view);
                    if let Some(displayed) = viewer.displayed_path.clone() {
                        tasks.push(fire_reupload_res(viewer, &displayed, Size::ZERO, true));
                    }
                    tasks.push(minimize);
                    return Task::batch(tasks);
                }
                minimize
            } else {
                // Arm the idle drop: fire once PREFETCH_IDLE elapses, tagged with
                // this window's new tier generation so a later focus or minimize
                // change supersedes it.
                win.tier_generation = win.tier_generation.wrapping_add(1);
                let generation = win.tier_generation;
                let arm = Task::future(async move {
                    tokio::time::sleep(PREFETCH_IDLE).await;
                    AppMessage::Window(Message::PrefetchIdle(generation))
                });
                Task::batch([arm, minimize])
            }
        }

        Message::PrefetchIdle(generation) => {
            // Shed the prefetch neighbors and demote the on-screen image to view
            // resolution, so this window holds nothing at full-res while it sits
            // in the background. A later focus or minimize change bumped the tier
            // generation, so a superseded timer no-ops.
            if generation != win.tier_generation {
                return Task::none();
            }
            let view = win.viewport_size;
            let Some(viewer) = win.viewer_mut() else {
                return Task::none();
            };
            viewer.drop_prefetch();
            if let Some(displayed) = viewer.displayed_path.clone() {
                return fire_reupload_res(viewer, &displayed, view, false);
            }
            Task::none()
        }

        Message::CheckMinimize => check_minimize(win.id),

        Message::Minimized(minimized) => {
            let changed = win.minimized != minimized;
            win.minimized = minimized;
            if !changed {
                return Task::none();
            }
            // A minimize or restore supersedes any pending unfocused idle-drop
            // timer, so it never demotes a window that is no longer just sitting
            // unfocused.
            win.tier_generation = win.tier_generation.wrapping_add(1);
            // Pause an open video the instant the window minimizes (audio stops
            // at once, not after the frame queue stalls), and resume on restore
            // only if it was playing.
            let mut resume = win.video_resumes_on_restore;
            if let Some(session) = win.viewer_mut().and_then(|v| v.video.as_mut()) {
                if minimized {
                    resume = session.playing;
                    session.pause();
                } else if resume {
                    session.play();
                    resume = false;
                }
            }
            win.video_resumes_on_restore = resume;

            // Release this window's GPU textures while it sits minimized, and
            // re-upload them from their RAM sources on restore (no disk read).
            let pipeline = shared.pipeline.clone();
            let view = win.viewport_size;
            match win.viewer_mut() {
                Some(viewer) if minimized => {
                    viewer.release_textures();
                    Task::none()
                }
                Some(viewer) => Task::batch(crate::app::update::fire_restore_textures(
                    &pipeline, viewer, view,
                )),
                None => Task::none(),
            }
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

/// Ask the windowing system for the window's maximized, minimized, and mode
/// state, so the size is persisted only when it is the plain windowed size.
fn check_window_state(id: iced::window::Id) -> Task<AppMessage> {
    iced::window::is_maximized(id).then(move |maximized| {
        iced::window::mode(id).then(move |mode| {
            iced::window::is_minimized(id).map(move |minimized| {
                AppMessage::Window(Message::WindowState {
                    maximized,
                    minimized: minimized.unwrap_or(false),
                    mode,
                })
            })
        })
    })
}

/// A size worth remembering only when the window is plainly windowed: neither
/// maximized, minimized, nor fullscreen.
fn should_persist(maximized: bool, minimized: bool, mode: iced::window::Mode) -> bool {
    !maximized && !minimized && mode == iced::window::Mode::Windowed
}

/// Query the OS minimize state. iced surfaces no minimize event on any backend
/// (it drops winit's `Occluded`), so this polls `is_minimized` on the focus
/// changes that bracket a minimize, plus a slow fallback for a minimize that
/// lands while the window is already unfocused. The poll is deliberate, not a
/// stopgap: the only event-driven alternative is a Windows-only 0x0 `Resized`,
/// and a one-platform event cannot be regression-tested in CI the way an
/// `is_minimized` poll can. The query reports on every backend but Wayland,
/// which has no way to know its own minimize state.
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
    fn minimizing_releases_the_cached_textures() {
        use crate::app::test_support::viewing_app;

        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");

        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));

        // The RAM source stays cached; only the GPU keepalive is released.
        let v = app.viewer().unwrap();
        assert!(v.cache.contains(std::path::Path::new("a.png")));
        assert!(
            v.cache
                .peek(std::path::Path::new("a.png"))
                .unwrap()
                .keepalive
                .is_none()
        );
    }

    fn cache_image(app: &mut crate::app::test_support::TestApp, path: &str) {
        use crate::app::state::CachedImage;
        let image = CachedImage {
            handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]),
            original_size: (2, 2),
            keepalive: Some(crate::ui::image_surface::test_keepalive()),
            gpu_full: true,
        };
        let cost = image.byte_cost();
        app.viewer_mut()
            .unwrap()
            .cache
            .insert(path.into(), image, cost);
    }

    #[test]
    fn focus_changes_advance_the_tier_generation() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let armed = app.window.tier_generation;
        // Losing focus arms an idle-drop timer under a fresh generation.
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        assert_ne!(app.window.tier_generation, armed);
        // Regaining focus supersedes it with another generation.
        let unfocused = app.window.tier_generation;
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(true));
        assert_ne!(app.window.tier_generation, unfocused);
    }

    #[test]
    fn the_idle_timer_sheds_prefetch_when_still_unfocused() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 1);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        cache_image(&mut app, "c.png");
        app.viewer_mut().unwrap().displayed_path = Some("b.png".into());

        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        let generation = app.window.tier_generation;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::PrefetchIdle(generation),
        );

        let v = app.viewer().unwrap();
        assert!(v.cache.contains(std::path::Path::new("b.png")));
        assert!(!v.cache.contains(std::path::Path::new("a.png")));
        assert!(!v.cache.contains(std::path::Path::new("c.png")));
    }

    #[test]
    fn a_stale_idle_timer_keeps_the_prefetch() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        // A refocus supersedes the drop; the old timer firing late must no-op.
        let stale = app.window.tier_generation;
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(true));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::PrefetchIdle(stale),
        );

        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains(std::path::Path::new("b.png"))
        );
    }

    #[test]
    fn minimizing_supersedes_the_pending_idle_drop() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        let armed = app.window.tier_generation;
        // Minimizing bumps the generation, so the idle-drop fired afterwards must
        // no-op rather than demote a window that is no longer merely unfocused.
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::PrefetchIdle(armed),
        );

        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains(std::path::Path::new("b.png"))
        );
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
                },
            );
        }
        assert_eq!(app.window.restored_size, Size::new(1024.0, 768.0));
        assert_eq!(app.window.restored_pos, iced::Point::new(120.0, 80.0));
    }

    #[test]
    fn only_a_normal_window_persists_its_size() {
        use iced::window::Mode;
        assert!(should_persist(false, false, Mode::Windowed));
        assert!(!should_persist(true, false, Mode::Windowed));
        assert!(!should_persist(false, true, Mode::Windowed));
        assert!(!should_persist(false, false, Mode::Fullscreen));
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
        // not overwrite it; only a close persists.
        app.window.window_size = Size::new(1024.0, 768.0);
        app.window.window_pos = iced::Point::new(500.0, 400.0);
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::WindowState {
                maximized: false,
                minimized: false,
                mode: iced::window::Mode::Windowed,
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
