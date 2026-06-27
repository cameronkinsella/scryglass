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
}

/// One stage of a backgrounded window's decay pipeline, run in this order
/// and each at its own delay from when the window entered the state. The same
/// three stages serve both the unfocused and minimized states; only their
/// configured timers differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecayStage {
    /// Demote the on-screen image to view-res and drop the prefetch look-ahead.
    Demote,
    /// Release all of this window's GPU textures (the RAM sources survive).
    DropVram,
    /// Evict this window's RAM sources, so they re-decode from disk on return.
    EvictRam,
}
use std::time::Duration;

use iced::Task;

use super::{fire_load, fire_prefetch, fire_restore_textures, fire_reupload_res};
use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{Message as AppMessage, Shared, Window, recalc_viewport};
use crate::media::pipeline::{Lane, Pipeline};

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
            // A focus change brackets a minimize/restore, so confirm the minimize
            // state on it instead of polling, and restart the decay pipeline.
            Task::batch([restart_decay(win, shared), check_minimize(win.id)])
        }

        Message::Decay { generation, stage } => run_decay_stage(win, shared, generation, stage),

        Message::Reactivate => restart_decay(win, shared),

        Message::CheckMinimize => check_minimize(win.id),

        Message::Minimized(minimized) => {
            let changed = win.minimized != minimized;
            win.minimized = minimized;
            if !changed {
                return Task::none();
            }
            // Pause an open video the instant the window minimizes (audio stops at
            // once, not after the frame queue stalls), and resume on restore only
            // if it was playing. The pause is opt-out via config; an unfocused but
            // un-minimized video keeps playing regardless.
            let pause = shared.config.resource.minimized.pause_video;
            let mut resume = win.video_resumes_on_restore;
            if let Some(session) = win.viewer_mut().and_then(|v| v.video.as_mut()) {
                if minimized && pause {
                    resume = session.playing;
                    session.pause();
                } else if !minimized && resume {
                    session.play();
                    resume = false;
                }
            }
            win.video_resumes_on_restore = resume;
            restart_decay(win, shared)
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

/// Run one decay stage, unless a focus or minimize change has since bumped
/// the generation it was armed under (then it is stale and no-ops).
fn run_decay_stage(
    win: &mut Window,
    shared: &mut Shared,
    generation: u64,
    stage: DecayStage,
) -> Task<AppMessage> {
    if generation != win.decay_generation {
        return Task::none();
    }
    let minimized = win.minimized;
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    match stage {
        DecayStage::Demote => {
            viewer.drop_prefetch();
            if let Some(displayed) = viewer.displayed_path.clone() {
                // Demote to the resolution of the current zoom, so a zoomed-in
                // background window stays as crisp as what is on screen.
                let zoom = viewer.zoom;
                return fire_reupload_res(viewer, &displayed, zoom, false);
            }
            Task::none()
        }
        DecayStage::DropVram => {
            // A visible window swaps to its thumbnail first so it does not blank;
            // a minimized window shows nothing, so it keeps the full display and
            // re-seats it on restore.
            if !minimized {
                viewer.swap_display_to_thumb();
            }
            viewer.release_textures();
            // The shed textures' dedup entries now hold their RAM for nothing.
            shared.pipeline.prune_dedup();
            Task::none()
        }
        DecayStage::EvictRam => {
            // Evicting the RAM means the on-screen image must let go of its full
            // handle too (even minimized, where it is not visible), or its pixels
            // stay alive. The thumbnail stands in until a return re-decodes.
            viewer.swap_display_to_thumb();
            viewer.evict_sources();
            shared.pipeline.prune_dedup();
            Task::none()
        }
    }
}

/// Re-evaluate the window's resource state after a focus or minimize change.
/// Bumps the decay generation (cancelling pending stage timers), restores the
/// display to what the new state needs, then arms each enabled decay stage
/// from config, clamped so the pipeline only ever runs forward.
fn restart_decay(win: &mut Window, shared: &mut Shared) -> Task<AppMessage> {
    win.decay_generation = win.decay_generation.wrapping_add(1);
    let generation = win.decay_generation;
    let pipeline = shared.pipeline.clone();
    let depth = shared.config.prefetch_depth;
    let prefetch_vram = shared.config.resource.prefetch_vram;
    let view = win.viewport_size;

    let mut tasks = restore_display(win, &pipeline, depth, view, prefetch_vram);

    // A focused window rests at full-res and reclaims nothing; a minimized window
    // takes precedence over focus.
    let cfg = if win.minimized {
        &shared.config.resource.minimized.pipeline
    } else if win.focused {
        return Task::batch(tasks);
    } else {
        &shared.config.resource.unfocused
    };

    // The eviction delay scales with the on-screen image's decode time.
    let decode = win.viewer().and_then(|v| {
        let path = v.displayed_path.as_deref()?;
        v.cache.peek(path)?.decode_time
    });
    for (stage, delay) in decay_schedule(cfg, decode) {
        tasks.push(arm_decay(generation, stage, delay));
    }
    Task::batch(tasks)
}

/// The enabled decay stages and their delays from state-entry, given a pipeline
/// config and the on-screen image's decode time. Each "never" stage is dropped,
/// and the surviving delays are clamped so a later stage never precedes an
/// earlier one. This is the pure decision behind `restart_decay`, so the
/// config-to-stages mapping is testable without a window or GPU.
fn decay_schedule(
    cfg: &crate::config::DecayPipeline,
    decode: Option<Duration>,
) -> Vec<(DecayStage, Duration)> {
    let (demote, drop_vram, evict) = clamp_decay(
        cfg.demote_vram_after,
        cfg.drop_vram_after,
        cfg.evict_delay(decode),
    );
    [
        (DecayStage::Demote, demote),
        (DecayStage::DropVram, drop_vram),
        (DecayStage::EvictRam, evict),
    ]
    .into_iter()
    .filter_map(|(stage, delay)| delay.map(|d| (stage, d)))
    .collect()
}

/// Restore the on-screen image to what the window's new state needs: re-decode
/// it if its RAM source was evicted, otherwise re-seat any released textures, and
/// for a focused window re-warm the prefetch look-ahead and promote the image to
/// full-res. A minimized window shows nothing, so nothing is restored.
fn restore_display(
    win: &mut Window,
    pipeline: &Pipeline,
    depth: usize,
    view: iced::Size,
    prefetch_vram: crate::config::PrefetchVram,
) -> Vec<Task<AppMessage>> {
    if win.minimized {
        return Vec::new();
    }
    let focused = win.focused;
    let Some(viewer) = win.viewer_mut() else {
        return Vec::new();
    };
    let displayed = viewer.displayed_path.clone();
    let evicted = displayed
        .as_ref()
        .is_some_and(|p| !viewer.cache.contains(p));

    let mut tasks = Vec::new();
    if evicted {
        // The thumbnail blur is already on screen; re-decode behind it.
        if let Some(p) = displayed.clone() {
            tasks.push(fire_load(
                pipeline,
                viewer,
                p,
                Lane::Current,
                view,
                prefetch_vram,
            ));
        }
    } else {
        tasks.extend(fire_restore_textures(pipeline, viewer, view));
        // Promote the on-screen image to full-res whether the window is focused
        // or just reactivated by a scroll, so the visible image is crisp; only
        // the prefetch look-ahead is focus-only.
        if let Some(p) = displayed.clone() {
            let zoom = viewer.zoom;
            tasks.push(fire_reupload_res(viewer, &p, zoom, true));
        }
    }
    if focused {
        tasks.extend(fire_prefetch(pipeline, viewer, depth, view, prefetch_vram));
    }
    tasks
}

/// Clamp the pipeline deadlines so a later stage never runs before an earlier
/// one: each enabled stage's effective delay is at least the previous enabled
/// stage's. So drop never precedes demote, nor evict drop, whatever the config
/// or the dynamic evict formula produced.
fn clamp_decay(
    demote: Option<Duration>,
    drop_vram: Option<Duration>,
    evict: Option<Duration>,
) -> (Option<Duration>, Option<Duration>, Option<Duration>) {
    let mut floor = Duration::ZERO;
    let mut clamp = |t: Option<Duration>| {
        t.map(|d| {
            floor = d.max(floor);
            floor
        })
    };
    (clamp(demote), clamp(drop_vram), clamp(evict))
}

/// Arm a single pipeline stage to fire after `delay`, tagged with the current
/// decay generation so a state change before it fires supersedes it.
fn arm_decay(generation: u64, stage: DecayStage, delay: Duration) -> Task<AppMessage> {
    Task::future(async move {
        tokio::time::sleep(delay).await;
        AppMessage::Window(Message::Decay { generation, stage })
    })
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

        // Minimizing arms the drop-VRAM stage (at 0s by default); fire it.
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        let generation = app.window.decay_generation;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::DropVram,
            },
        );

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
            decode_time: None,
        };
        let cost = image.byte_cost();
        app.viewer_mut()
            .unwrap()
            .cache
            .insert(path.into(), image, cost);
    }

    #[test]
    fn focus_changes_advance_the_decay_generation() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let armed = app.window.decay_generation;
        // Losing focus arms an idle-drop timer under a fresh generation.
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        assert_ne!(app.window.decay_generation, armed);
        // Regaining focus supersedes it with another generation.
        let unfocused = app.window.decay_generation;
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(true));
        assert_ne!(app.window.decay_generation, unfocused);
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
        let generation = app.window.decay_generation;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::Demote,
            },
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
        let stale = app.window.decay_generation;
        let _ = update(&mut app.window, &mut app.shared, Message::Focused(true));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation: stale,
                stage: DecayStage::Demote,
            },
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
        let armed = app.window.decay_generation;
        // Minimizing bumps the generation, so the idle-drop fired afterwards must
        // no-op rather than demote a window that is no longer merely unfocused.
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation: armed,
                stage: DecayStage::Demote,
            },
        );

        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains(std::path::Path::new("b.png"))
        );
    }

    #[test]
    fn the_evict_stage_clears_the_window_cache() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        // Minimized, so no thumbnail swap is needed for the on-screen image.
        app.window.minimized = true;
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

        let generation = app.window.decay_generation;
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::EvictRam,
            },
        );

        let v = app.viewer().unwrap();
        assert!(!v.cache.contains(std::path::Path::new("a.png")));
        assert!(!v.cache.contains(std::path::Path::new("b.png")));
    }

    #[test]
    fn clamp_decay_never_runs_a_later_stage_first() {
        let s = Duration::from_secs;
        // A drop and evict configured before the demote are clamped up to it.
        assert_eq!(
            clamp_decay(Some(s(15)), Some(s(5)), Some(s(1))),
            (Some(s(15)), Some(s(15)), Some(s(15)))
        );
        // A disabled stage is skipped but still raises the floor for later ones.
        assert_eq!(
            clamp_decay(Some(s(10)), None, Some(s(3))),
            (Some(s(10)), None, Some(s(10)))
        );
        // Already-ordered timers pass through unchanged.
        assert_eq!(
            clamp_decay(Some(s(5)), Some(s(10)), Some(s(20))),
            (Some(s(5)), Some(s(10)), Some(s(20)))
        );
    }

    #[test]
    fn decay_schedule_maps_config_to_enabled_clamped_stages() {
        use crate::config::{DecayPipeline, EvictPolicy};
        let s = Duration::from_secs;

        // Demote plus a fixed evict; the disabled ("never") drop is skipped, and
        // the evict is clamped to land no earlier than the demote.
        let cfg = DecayPipeline {
            demote_vram_after: Some(s(15)),
            drop_vram_after: None,
            evict_ram: EvictPolicy::Fixed(s(60)),
            ..Default::default()
        };
        assert_eq!(
            decay_schedule(&cfg, None),
            vec![(DecayStage::Demote, s(15)), (DecayStage::EvictRam, s(60))]
        );

        // evict_ram = "never" drops the evict stage entirely.
        let cfg = DecayPipeline {
            evict_ram: EvictPolicy::Never,
            ..cfg
        };
        assert_eq!(
            decay_schedule(&cfg, None),
            vec![(DecayStage::Demote, s(15))]
        );

        // A drop configured before the demote is clamped up to it, order kept.
        let cfg = DecayPipeline {
            demote_vram_after: Some(s(15)),
            drop_vram_after: Some(s(5)),
            evict_ram: EvictPolicy::Never,
            ..Default::default()
        };
        assert_eq!(
            decay_schedule(&cfg, None),
            vec![(DecayStage::Demote, s(15)), (DecayStage::DropVram, s(15))]
        );

        // Everything disabled: nothing decays.
        let cfg = DecayPipeline {
            demote_vram_after: None,
            drop_vram_after: None,
            evict_ram: EvictPolicy::Never,
            ..Default::default()
        };
        assert!(decay_schedule(&cfg, None).is_empty());

        // Dynamic eviction needs a measured decode time; without one it never
        // evicts (conservative), and with one it schedules a single evict stage.
        let cfg = DecayPipeline {
            demote_vram_after: None,
            drop_vram_after: None,
            evict_ram: EvictPolicy::Dynamic,
            ..Default::default()
        };
        assert!(decay_schedule(&cfg, None).is_empty());
        let sched = decay_schedule(&cfg, Some(std::time::Duration::from_millis(20)));
        assert_eq!(sched.len(), 1);
        assert_eq!(sched[0].0, DecayStage::EvictRam);
    }

    #[test]
    fn decay_stages_free_the_expected_bytes() {
        use crate::app::test_support::viewing_app;
        // Three 2x2 RGBA images, 16 bytes of RAM each, cached and resident.
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 1);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        cache_image(&mut app, "c.png");
        app.viewer_mut().unwrap().displayed_path = Some("b.png".into());
        let generation = app.window.decay_generation;
        assert_eq!(app.viewer().unwrap().cache.used_bytes(), 48);

        // Demote sheds the two prefetch neighbors' RAM, keeps the on-screen image.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::Demote,
            },
        );
        assert_eq!(app.viewer().unwrap().cache.used_bytes(), 16);

        // EvictRam frees the rest: no RAM held for this window.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::EvictRam,
            },
        );
        assert_eq!(app.viewer().unwrap().cache.used_bytes(), 0);
    }

    #[test]
    fn drop_vram_releases_textures_but_keeps_ram() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        let generation = app.window.decay_generation;
        let ram = app.viewer().unwrap().cache.used_bytes();

        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::DropVram,
            },
        );

        let v = app.viewer().unwrap();
        // RAM is untouched; every GPU keepalive (the resident texture) is gone.
        assert_eq!(v.cache.used_bytes(), ram);
        assert!(v.cache.iter().all(|(_, img)| img.keepalive.is_none()));
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
