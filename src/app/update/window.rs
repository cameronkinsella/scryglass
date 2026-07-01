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
    /// The settle timer after a move or resize fired; a later event bumps the
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

/// One stage of a backgrounded window's decay pipeline, run in this order
/// and each at its own delay from when the window entered the state. The same
/// stages serve both the unfocused and minimized states; only their configured
/// timers differ. Which stages apply depends on the on-screen media: a still runs
/// demote/drop/evict, an animation only evict, a video only its session release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecayStage {
    /// Demote the on-screen image to view-res.
    Demote,
    /// Drop the on-screen image's GPU texture (its RAM source survives).
    DropVram,
    /// Evict this window's RAM sources, so they re-decode from disk on return.
    EvictRam,
    /// Release an open video's whole decode session, freezing the last frame; a
    /// restore re-opens it at the saved position.
    EvictVideo,
    /// Release the furthest remaining ring of prefetched neighbors, then re-arm
    /// itself after the configured interval while any remain. Started by its
    /// configured anchor event (state entry or one of the stages above), so it
    /// runs beside the on-screen pipeline, not inside it.
    ShedPrefetch,
}
use std::time::Duration;

use iced::{Size, Task};

use super::{fire_load, fire_prefetch, fire_rotate, run_jobs_at, try_start_shared_anim};
use crate::app::state::{DisplayedImage, Viewer};
use crate::app::viewer_math::{clamp_pan, compute_zoom};
use crate::app::{Message as AppMessage, Shared, Window, recalc_viewport};
use crate::config::{EvictConfig, PrefetchDecay, PrefetchDropAnchor, PrefetchVram, VideoDecay};
use crate::media::pipeline::{Lane, Pipeline};
use crate::media::store::{ImageKey, Tier};

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

            // A resize changes the placement like a zoom, so its demand is
            // debounced too: it must run after a frame draws (and stamps)
            // the new geometry, and a live resize storm coalesces to one.
            let tiles = super::settle_tiles(win);
            // The app's own fullscreen never persists. A natively maximized or
            // fullscreened window looks like any other resize here, so confirm
            // the state before persisting and let the windowed size stand.
            if win.fullscreen {
                tiles
            } else {
                Task::batch([tiles, debounce_window_state(win)])
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
            restart_decay(win, shared)
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
/// geometry save. A drag streams events; probing only after they settle keeps
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
fn should_persist(maximized: bool, minimized: bool, mode: iced::window::Mode, snapped: bool) -> bool {
    !maximized && !minimized && !snapped && mode == iced::window::Mode::Windowed
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
    // The shedding walks in one ring per firing: release the furthest ring and,
    // while any neighbor remains, come back after the configured interval.
    if stage == DecayStage::ShedPrefetch {
        let interval = if win.minimized {
            shared.config.resource.minimized.prefetch.drop_interval
        } else {
            shared.config.resource.unfocused.prefetch.drop_interval
        };
        let more = win.viewer_mut().is_some_and(Viewer::drop_prefetch_ring);
        if more {
            return arm_decay(generation, DecayStage::ShedPrefetch, interval);
        }
        return Task::none();
    }
    // Releasing a video session touches both the viewer and the window's resume flag,
    // so handle it before borrowing the viewer for the store-backed (still) stages.
    if stage == DecayStage::EvictVideo {
        let resume = win.video_resumes_on_restore;
        let minimized = win.minimized;
        if let Some(viewer) = win.viewer_mut() {
            // Capture the session into a memo (which keeps the archive temp file alive)
            // and drop it, freeing the decode threads, hardware decoder, audio sink,
            // and GPU textures. The last `frame` stays on screen, so the video looks
            // paused, not gone, until the window returns. `playing` resolves through
            // the minimize-pause flag, which may have already cleared `session.playing`.
            let memo = viewer
                .video
                .session
                .as_ref()
                .map(|s| s.suspend(s.playing || resume));
            if memo.is_some() {
                viewer.video.suspended = memo;
                viewer.video.session = None;
                // A minimized window shows nothing, so also drop the frozen frame: its
                // RAM and the GPU surface it pins are pure waste while hidden, and a
                // restore re-opens the session and repaints. A visible (unfocused)
                // release keeps the frame on screen instead.
                if minimized {
                    viewer.video.frame = None;
                }
                // TODO: compact the process heap here (Windows) so the freed decoder
                // RAM returns to the OS instead of staying mapped until idle.
            }
        }
        win.video_resumes_on_restore = false;
        return Task::none();
    }
    let view = win.viewport_size;
    let minimized = win.minimized;
    let pipeline = shared.pipeline.clone();
    let Some(viewer) = win.viewer_mut() else {
        return Task::none();
    };
    // A minimized window shows nothing, so its rotation override (a full
    // texture the store does not govern) is pure waste; a restore re-derives
    // it. A visible window keeps it, or the image would snap unrotated.
    if minimized
        && let DisplayedImage::Full { rotated, .. } = &mut viewer.displayed
        && rotated.is_some()
    {
        *rotated = None;
        viewer.displayed_rotation = 0;
    }
    // Each stage lowers this window's demand on its leases. The store frees an
    // image only once no window still wants it higher, so a shared image a
    // focused window holds never blanks from a background window's decay.
    match stage {
        DecayStage::Demote => {
            // Demote the on-screen image to a smaller view-res texture.
            retarget_displayed(viewer, shared, Tier::View, &pipeline, view)
        }
        DecayStage::DropVram => {
            // Drop the texture; the view falls back to the thumbnail blur, and a
            // refocus re-uploads from the RAM source with no disk read.
            retarget_displayed(viewer, shared, Tier::InRam, &pipeline, view)
        }
        DecayStage::EvictRam => {
            let is_anim = matches!(viewer.displayed, DisplayedImage::Animated { .. });
            if is_anim {
                // Lower this window's demand on the shared frames to evicted, exactly
                // like a still lowering its tier, and nothing more. The lease and the
                // (now dormant) playback are kept, so the store's aggregate demand
                // alone decides residency: the frames free only once every window has
                // dropped to evicted, and they all evict together. When any window
                // brings them back, every window's dormant playback resumes from where
                // it was. That makes decay state shared across windows, like stills.
                if let Some(path) = viewer.displayed_path.clone()
                    && let Some(lease) = viewer.anim_player.lease(&path)
                {
                    shared.anim_store.retarget(lease, Tier::Evicted);
                }
                Task::none()
            } else {
                // A still lowers its demand to evicted: the store frees its RAM once
                // no window wants it higher. While another window holds the image it
                // stays sharp; once the last holder releases it the cell empties and
                // the view falls back to the blur. A return re-decodes.
                retarget_displayed(viewer, shared, Tier::Evicted, &pipeline, view)
            }
        }
        DecayStage::EvictVideo | DecayStage::ShedPrefetch => {
            unreachable!("handled before the viewer borrow above")
        }
    }
}

/// When the prefetch shedding starts, measured from state entry: the anchor
/// resolves to the earliest armed stage at or after it in pipeline order, plus
/// the configured delay. A skipped anchor stage falls through to the next one
/// that actually runs, so `drop_on = "demote"` still sheds when only the drop
/// stage is enabled. `None` (keep the prefetch) only when nothing at or after
/// the anchor runs.
fn shed_start(cfg: &PrefetchDecay, stages: &[(DecayStage, Duration)]) -> Option<Duration> {
    let anchor = match cfg.drop_on {
        PrefetchDropAnchor::Immediately => return Some(cfg.drop_after),
        PrefetchDropAnchor::Demote => 0,
        PrefetchDropAnchor::Drop => 1,
        PrefetchDropAnchor::Evict => 2,
    };
    stages
        .iter()
        .filter(|(stage, _)| stage_rank(*stage) >= anchor)
        .map(|(_, delay)| *delay + cfg.drop_after)
        .min()
}

/// A stage's position in the decay pipeline, for the anchor fall-through. A
/// video's session release stands in for its evict.
fn stage_rank(stage: DecayStage) -> u8 {
    match stage {
        DecayStage::Demote => 0,
        DecayStage::DropVram => 1,
        DecayStage::EvictRam | DecayStage::EvictVideo => 2,
        // Never armed by the still/anim/video schedules, so never an anchor.
        DecayStage::ShedPrefetch => u8::MAX,
    }
}

/// Lower the on-screen image's lease to `tier`, firing any re-mint the store asks
/// for (a smaller texture when demoting full to view). A no-op when nothing is on
/// screen or it is not leased by this window.
fn retarget_displayed(
    viewer: &mut Viewer,
    shared: &mut Shared,
    tier: Tier,
    pipeline: &Pipeline,
    view: Size,
) -> Task<AppMessage> {
    let Some(path) = viewer.displayed_path.clone() else {
        return Task::none();
    };
    // The view-res copy is sized to the current zoom, so a zoomed-in image stays as
    // crisp as it is on screen rather than falling back to its fit resolution.
    let zoom = viewer.zoom;
    let Some(lease) = viewer.cache.get(&path) else {
        return Task::none();
    };
    let outcome = shared.store.retarget(lease, tier);
    run_jobs_at(outcome.jobs, pipeline, Lane::Prefetch, view, Some(zoom))
}

/// Re-evaluate the window's resource state after a focus or minimize change.
/// Bumps the decay generation (cancelling pending stage timers), restores the
/// display to what the new state needs, then arms each enabled decay stage
/// from config, clamped so the pipeline only ever runs forward.
fn restart_decay(win: &mut Window, shared: &mut Shared) -> Task<AppMessage> {
    win.decay_generation = win.decay_generation.wrapping_add(1);
    let generation = win.decay_generation;
    let depth = shared.config.prefetch_depth;
    let prefetch_vram = shared.config.resource.prefetch_vram;
    let view = win.viewport_size;

    let mut tasks = restore_display(win, shared, depth, view, prefetch_vram);

    // A focused, non-minimized window rests at full-res and reclaims nothing.
    if win.focused && !win.minimized {
        return Task::batch(tasks);
    }
    // A video on screen owns the window's decay (it is keyed off the session, alive
    // or suspended, since a video minimized during warmup never latches `displayed`).
    let displayed_is_video = win
        .viewer()
        .is_some_and(|v| v.video.session.is_some() || v.video.suspended.is_some());
    let displayed_is_anim = win
        .viewer()
        .is_some_and(|v| matches!(v.displayed, DisplayedImage::Animated { .. }));

    // The eviction delay scales with the on-screen image's decode time, measured at
    // decode by whichever store owns it (the animation store for a GIF). A video has
    // no decode-time-scaled timer, so this is unused for it.
    let decode = win.viewer().and_then(|v| {
        let path = v.displayed_path.as_deref()?;
        let key = ImageKey::new(&v.source, path);
        if displayed_is_anim {
            shared.anim_store.decode_time(&key)
        } else {
            shared.store.decode_time(&key)
        }
    });

    // Each media kind decays differently and they are mutually exclusive on screen: a
    // video releases its whole decode session, an animation evicts its RAM frames, a
    // still runs the full demote/drop/evict pipeline.
    let res = &shared.config.resource;
    let stages = if displayed_is_video {
        let cfg = if win.minimized {
            &res.minimized.video
        } else {
            &res.unfocused.video
        };
        video_decay_schedule(cfg)
    } else if displayed_is_anim {
        let cfg = if win.minimized {
            &res.minimized.animated
        } else {
            &res.unfocused.animated
        };
        anim_decay_schedule(cfg, decode)
    } else {
        let cfg = if win.minimized {
            &res.minimized.still
        } else {
            &res.unfocused.still
        };
        decay_schedule(cfg, decode)
    };
    // The shedding's start is resolved against the stages that actually run
    // (a skipped anchor falls through), so it is armed here as one absolute
    // timer from state entry, like the stages themselves.
    let prefetch = if win.minimized {
        &res.minimized.prefetch
    } else {
        &res.unfocused.prefetch
    };
    if let Some(delay) = shed_start(prefetch, &stages) {
        tasks.push(arm_decay(generation, DecayStage::ShedPrefetch, delay));
    }
    for (stage, delay) in stages {
        tasks.push(arm_decay(generation, stage, delay));
    }
    Task::batch(tasks)
}

/// The evict stage and its delay for an animation, given its `.animated` config.
/// An animation has no VRAM tier, so eviction is the only stage it can ever run;
/// `EvictConfig` makes demote/drop unrepresentable, so there is nothing to clamp.
fn anim_decay_schedule(cfg: &EvictConfig, decode: Option<Duration>) -> Vec<(DecayStage, Duration)> {
    cfg.evict_delay(decode)
        .map(|d| (DecayStage::EvictRam, d))
        .into_iter()
        .collect()
}

/// The session-release stage and its delay for a video, given its `.video` config.
/// A video has no VRAM or RAM tier the store governs, so releasing the whole decode
/// session is the only stage it can ever run.
fn video_decay_schedule(cfg: &VideoDecay) -> Vec<(DecayStage, Duration)> {
    cfg.evict_session_after
        .map(|d| (DecayStage::EvictVideo, d))
        .into_iter()
        .collect()
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
        cfg.evict.evict_delay(decode),
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

/// Restore the on-screen image to what the window's new state needs: bring it
/// back to a full-res texture (re-uploading from RAM, or re-decoding if it was
/// evicted), and for a focused window re-warm the prefetch look-ahead. The
/// display stays put; the view re-sharpens once the texture lands. A minimized
/// window shows nothing, so nothing is restored.
fn restore_display(
    win: &mut Window,
    shared: &mut Shared,
    depth: usize,
    view: Size,
    prefetch_vram: PrefetchVram,
) -> Vec<Task<AppMessage>> {
    if win.minimized {
        return Vec::new();
    }
    let focused = win.focused;
    let pipeline = shared.pipeline.clone();
    let Some(viewer) = win.viewer_mut() else {
        return Vec::new();
    };
    let mut tasks = Vec::new();
    // A video released while backgrounded re-opens at its saved position now that the
    // window has returned. The frozen frame is already on screen, so the next frame
    // tick (re-armed once the session exists) paints the fresh one over it; the first
    // poll delivers it even while paused, so a paused video shows the right frame.
    if let Some(memo) = viewer.video.suspended.take() {
        viewer.video.session = Some(crate::video::VideoSession::resume(&memo));
    }
    if let Some(displayed) = viewer.displayed_path.clone() {
        let is_anim = matches!(viewer.displayed, DisplayedImage::Animated { .. });
        if is_anim {
            // Re-lease the shared frames if any window still has them resident (no
            // decode); otherwise re-decode through the still path, which re-discovers
            // the animation and re-registers it in the shared store.
            if let Some(anim_task) =
                try_start_shared_anim(&mut shared.anim_store, viewer, &displayed)
            {
                tasks.push(anim_task.map(AppMessage::Anim));
            } else {
                tasks.push(fire_load(
                    &mut shared.store,
                    &pipeline,
                    viewer,
                    displayed,
                    Tier::Full,
                    view,
                ));
            }
        } else {
            tasks.push(fire_load(
                &mut shared.store,
                &pipeline,
                viewer,
                displayed,
                Tier::Full,
                view,
            ));
            // Re-derive a rotation override the minimized decay dropped
            tasks.push(fire_rotate(viewer, &shared.store));
        }
    }
    // A focused window re-warms its look-ahead; a window only reactivated by a
    // scroll restores just the visible image.
    if focused {
        tasks.extend(fire_prefetch(
            &mut shared.store,
            &pipeline,
            viewer,
            depth,
            view,
            prefetch_vram,
        ));
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
    use crate::app::test_support::{cache_image, empty_app};

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
    fn minimizing_drops_the_texture_but_keeps_the_ram() {
        use crate::app::test_support::viewing_app;

        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

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

        // The image stays leased with its RAM in the store; only the shared
        // texture is dropped, so the lease now reads no texture.
        let key = ImageKey::new(
            &crate::media::pipeline::Source::Fs,
            std::path::Path::new("a.png"),
        );
        assert!(app.shared.store.tier(&key) >= Tier::InRam);
        let v = app.viewer().unwrap();
        assert!(v.cache.contains_key(std::path::Path::new("a.png")));
        assert!(
            v.cache
                .get(std::path::Path::new("a.png"))
                .unwrap()
                .texture()
                .is_none()
        );
    }

    #[test]
    fn minimizing_drops_the_rotation_override() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        let viewer = app.viewer_mut().unwrap();
        viewer.displayed_path = Some("a.png".into());
        viewer.displayed = DisplayedImage::Full {
            original_size: (2, 2),
            rotated: Some(crate::ui::image_surface::test_keepalive()),
        };
        viewer.rotation = 1;
        viewer.displayed_rotation = 1;

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

        // The override's texture is freed; the wanted rotation survives so a
        // restore re-derives it.
        let v = app.viewer().unwrap();
        assert!(matches!(
            v.displayed,
            DisplayedImage::Full { rotated: None, .. }
        ));
        assert_eq!(v.displayed_rotation, 0);
        assert_eq!(v.rotation, 1);
    }

    #[test]
    fn an_unfocused_window_keeps_the_rotation_override() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        let viewer = app.viewer_mut().unwrap();
        viewer.displayed_path = Some("a.png".into());
        viewer.displayed = DisplayedImage::Full {
            original_size: (2, 2),
            rotated: Some(crate::ui::image_surface::test_keepalive()),
        };
        viewer.rotation = 1;
        viewer.displayed_rotation = 1;

        // Still visible: dropping the override would snap the image unrotated.
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
        assert!(matches!(
            v.displayed,
            DisplayedImage::Full {
                rotated: Some(_),
                ..
            }
        ));
        assert_eq!(v.displayed_rotation, 1);
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
    fn the_shed_timer_drops_prefetch_when_still_unfocused() {
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
                stage: DecayStage::ShedPrefetch,
            },
        );

        // Both neighbors sit one step out, so the one ring covers them.
        let v = app.viewer().unwrap();
        assert!(v.cache.contains_key(std::path::Path::new("b.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("a.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("c.png")));
    }

    #[test]
    fn shedding_walks_in_from_the_furthest_ring_while_minimized() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png", "c.png", "d.png", "e.png"], 2);
        for name in ["a.png", "b.png", "c.png", "d.png", "e.png"] {
            cache_image(&mut app, name);
        }
        app.viewer_mut().unwrap().displayed_path = Some("c.png".into());

        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        let generation = app.window.decay_generation;
        let shed = Message::Decay {
            generation,
            stage: DecayStage::ShedPrefetch,
        };

        // First firing: only the outermost pair goes.
        let _ = update(&mut app.window, &mut app.shared, shed.clone());
        let v = app.viewer().unwrap();
        assert!(!v.cache.contains_key(std::path::Path::new("a.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("e.png")));
        assert!(v.cache.contains_key(std::path::Path::new("b.png")));
        assert!(v.cache.contains_key(std::path::Path::new("d.png")));

        // Second firing: the next ring goes, the on-screen image stays.
        let _ = update(&mut app.window, &mut app.shared, shed);
        let v = app.viewer().unwrap();
        assert!(!v.cache.contains_key(std::path::Path::new("b.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("d.png")));
        assert!(v.cache.contains_key(std::path::Path::new("c.png")));
    }

    #[test]
    fn shed_start_resolves_its_anchor_against_the_armed_stages() {
        use crate::config::{PrefetchDecay, PrefetchDropAnchor};
        let s = Duration::from_secs;
        let cfg = |drop_on| PrefetchDecay {
            drop_on,
            drop_after: s(3),
            drop_interval: s(1),
        };
        let full = [
            (DecayStage::Demote, s(10)),
            (DecayStage::DropVram, s(20)),
            (DecayStage::EvictRam, s(30)),
        ];

        // Each anchor picks its own stage when armed.
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Demote), &full), Some(s(13)));
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Drop), &full), Some(s(23)));
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Evict), &full), Some(s(33)));

        // A skipped anchor falls through to the next stage that runs: demote
        // disabled, drop at 0s (the default minimized pipeline).
        let drop_only = [(DecayStage::DropVram, Duration::ZERO)];
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Demote), &drop_only),
            Some(s(3))
        );

        // Nothing at or after the anchor runs: the prefetch is kept.
        let demote_only = [(DecayStage::Demote, s(15))];
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Evict), &demote_only), None);
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Demote), &[]), None);

        // A video's session release stands in for its evict, and earlier
        // anchors fall through to it.
        let video = [(DecayStage::EvictVideo, s(5))];
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Demote), &video), Some(s(8)));
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Evict), &video), Some(s(8)));

        // Immediately counts from state entry, whatever the pipeline runs.
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Immediately), &[]), Some(s(3)));
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
                stage: DecayStage::ShedPrefetch,
            },
        );

        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains_key(std::path::Path::new("b.png"))
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
        // Minimizing bumps the generation, so a shed armed under the unfocused
        // state must no-op when it fires late: the minimized state armed its own.
        let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation: armed,
                stage: DecayStage::ShedPrefetch,
            },
        );

        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains_key(std::path::Path::new("b.png"))
        );
    }

    #[test]
    fn the_evict_stage_frees_the_displayed_ram_and_leaves_prefetch_alone() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
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

        // The on-screen image keeps its lease (so it still reads the shared
        // cell) but its RAM is freed, since no window wants it any higher. The
        // neighbor is untouched: shedding the look-ahead belongs to the
        // prefetch schedule, not this stage.
        let a_key = ImageKey::new(
            &crate::media::pipeline::Source::Fs,
            std::path::Path::new("a.png"),
        );
        assert!(app.shared.store.tier(&a_key) < Tier::InRam);
        let v = app.viewer().unwrap();
        assert!(v.cache.contains_key(std::path::Path::new("a.png")));
        assert!(v.cache.contains_key(std::path::Path::new("b.png")));
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
        use crate::config::{DecayPipeline, EvictConfig, EvictPolicy};
        let s = Duration::from_secs;
        let pipe = |demote, drop_vram, evict_ram| DecayPipeline {
            demote_vram_after: demote,
            drop_vram_after: drop_vram,
            evict: EvictConfig {
                evict_ram,
                ..Default::default()
            },
        };

        // Demote plus a fixed evict; the disabled ("never") drop is skipped, and
        // the evict is clamped to land no earlier than the demote.
        assert_eq!(
            decay_schedule(&pipe(Some(s(15)), None, EvictPolicy::Fixed(s(60))), None),
            vec![(DecayStage::Demote, s(15)), (DecayStage::EvictRam, s(60))]
        );

        // evict_ram = "never" drops the evict stage entirely.
        assert_eq!(
            decay_schedule(&pipe(Some(s(15)), None, EvictPolicy::Never), None),
            vec![(DecayStage::Demote, s(15))]
        );

        // A drop configured before the demote is clamped up to it, order kept.
        assert_eq!(
            decay_schedule(&pipe(Some(s(15)), Some(s(5)), EvictPolicy::Never), None),
            vec![(DecayStage::Demote, s(15)), (DecayStage::DropVram, s(15))]
        );

        // Everything disabled: nothing decays.
        assert!(decay_schedule(&pipe(None, None, EvictPolicy::Never), None).is_empty());

        // Dynamic eviction needs a measured decode time; without one it never
        // evicts (conservative), and with one it schedules a single evict stage.
        let dynamic = pipe(None, None, EvictPolicy::Dynamic);
        assert!(decay_schedule(&dynamic, None).is_empty());
        let sched = decay_schedule(&dynamic, Some(Duration::from_millis(20)));
        assert_eq!(sched.len(), 1);
        assert_eq!(sched[0].0, DecayStage::EvictRam);
    }

    #[test]
    fn anim_decay_schedule_arms_only_the_evict_stage() {
        use crate::config::EvictPolicy;
        let s = Duration::from_secs;

        // An animation's `.animated` config is an `EvictConfig`: it has no demote or
        // drop to represent, so the schedule is only ever a single evict stage.
        let cfg = EvictConfig {
            evict_ram: EvictPolicy::Fixed(s(30)),
            ..Default::default()
        };
        assert_eq!(
            anim_decay_schedule(&cfg, None),
            vec![(DecayStage::EvictRam, s(30))]
        );

        // "never" arms nothing, so a backgrounded animation just keeps its frames.
        let cfg = EvictConfig {
            evict_ram: EvictPolicy::Never,
            ..Default::default()
        };
        assert!(anim_decay_schedule(&cfg, None).is_empty());
    }

    #[test]
    fn video_decay_schedule_arms_only_the_session_release() {
        use crate::config::VideoDecay;
        let s = Duration::from_secs;
        // A video has no tier, so its schedule is at most one session-release stage.
        assert_eq!(
            video_decay_schedule(&VideoDecay {
                evict_session_after: Some(s(5)),
            }),
            vec![(DecayStage::EvictVideo, s(5))]
        );
        // "never" arms nothing, so a backgrounded video keeps its decode session.
        assert!(
            video_decay_schedule(&VideoDecay {
                evict_session_after: None,
            })
            .is_empty()
        );
    }

    // The video session lifecycle is exercised on the stub (the real `open` spawns
    // FFmpeg threads); the wiring it tests is identical in both builds.
    #[cfg(not(feature = "video"))]
    mod video {
        use super::*;
        use crate::app::test_support::{TestApp, viewing_app};
        use std::sync::Arc;

        fn stub_frame() -> crate::video::VideoFrame {
            crate::video::VideoFrame {
                id: 1,
                width: 2,
                height: 2,
                chroma_width: 1,
                chroma_height: 1,
                format: crate::video::YuvFormat::I420,
                y: vec![0; 4],
                u: vec![0; 1],
                v: vec![0; 1],
                matrix: crate::video::YuvMatrix::Bt601,
                range: crate::video::YuvRange::Limited,
                timestamp: Duration::ZERO,
            }
        }

        fn app_with_video() -> TestApp {
            let mut app = viewing_app(&["a.mp4"], 0);
            let viewer = app.viewer_mut().unwrap();
            viewer.displayed_path = Some("a.mp4".into());
            let mut session = crate::video::VideoSession::open(
                "a.mp4".into(),
                Duration::ZERO,
                1.0,
                false,
                false,
                false,
            );
            session.playing = true;
            viewer.video.session = Some(session);
            viewer.video.frame = Some(Arc::new(stub_frame()));
            app
        }

        #[test]
        fn minimizing_then_evicting_releases_the_session_and_frame() {
            let mut app = app_with_video();
            let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
            let generation = app.window.decay_generation;
            let _ = update(
                &mut app.window,
                &mut app.shared,
                Message::Decay {
                    generation,
                    stage: DecayStage::EvictVideo,
                },
            );
            let v = app.viewer().unwrap();
            assert!(v.video.session.is_none()); // decoder released
            assert!(v.video.suspended.is_some()); // memo kept for restore
            // A minimized window is hidden, so the frozen frame is dropped too.
            assert!(v.video.frame.is_none());
        }

        #[test]
        fn a_stale_evict_video_keeps_the_session() {
            let mut app = app_with_video();
            let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
            // Generation 0 predates the minimize bump, so the timer is stale.
            let _ = update(
                &mut app.window,
                &mut app.shared,
                Message::Decay {
                    generation: 0,
                    stage: DecayStage::EvictVideo,
                },
            );
            assert!(app.viewer().unwrap().video.session.is_some());
        }

        #[test]
        fn restoring_re_opens_a_suspended_video() {
            let mut app = app_with_video();
            let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
            let generation = app.window.decay_generation;
            let _ = update(
                &mut app.window,
                &mut app.shared,
                Message::Decay {
                    generation,
                    stage: DecayStage::EvictVideo,
                },
            );
            assert!(app.viewer().unwrap().video.suspended.is_some());

            // Un-minimize: restore_display re-opens the session at the saved position.
            let _ = update(&mut app.window, &mut app.shared, Message::Minimized(false));
            let v = app.viewer().unwrap();
            assert!(v.video.session.is_some());
            assert!(v.video.suspended.is_none());
        }

        #[test]
        fn reset_clears_a_suspended_video() {
            let mut app = app_with_video();
            let _ = update(&mut app.window, &mut app.shared, Message::Minimized(true));
            let generation = app.window.decay_generation;
            let _ = update(
                &mut app.window,
                &mut app.shared,
                Message::Decay {
                    generation,
                    stage: DecayStage::EvictVideo,
                },
            );
            assert!(app.viewer().unwrap().video.suspended.is_some());
            // Navigating away (reset) releases the memo and its archive temp guard.
            app.viewer_mut().unwrap().video.reset();
            assert!(app.viewer().unwrap().video.suspended.is_none());
        }
    }

    #[test]
    fn decay_sheds_prefetch_then_evicts_the_rest() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 1);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        cache_image(&mut app, "c.png");
        app.viewer_mut().unwrap().displayed_path = Some("b.png".into());
        let generation = app.window.decay_generation;

        // The shed releases the two prefetch neighbors, keeping the on-screen
        // image (both sit one step out, so one ring covers them).
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::ShedPrefetch,
            },
        );
        let v = app.viewer().unwrap();
        assert!(v.cache.contains_key(std::path::Path::new("b.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("a.png")));
        assert!(!v.cache.contains_key(std::path::Path::new("c.png")));

        // EvictRam frees the on-screen image's RAM while keeping its lease, so it
        // can still read the shared cell if another window holds the texture.
        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::EvictRam,
            },
        );
        let b_key = ImageKey::new(
            &crate::media::pipeline::Source::Fs,
            std::path::Path::new("b.png"),
        );
        assert!(app.shared.store.tier(&b_key) < Tier::InRam);
        assert!(
            app.viewer()
                .unwrap()
                .cache
                .contains_key(std::path::Path::new("b.png"))
        );
    }

    #[test]
    fn drop_vram_drops_the_texture_but_keeps_ram() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());
        let generation = app.window.decay_generation;

        let _ = update(
            &mut app.window,
            &mut app.shared,
            Message::Decay {
                generation,
                stage: DecayStage::DropVram,
            },
        );

        // The texture is gone, but the RAM source survives in the store, so a
        // refocus re-uploads with no disk read.
        let key = ImageKey::new(
            &crate::media::pipeline::Source::Fs,
            std::path::Path::new("a.png"),
        );
        assert!(app.shared.store.tier(&key) >= Tier::InRam);
        let v = app.viewer().unwrap();
        assert!(
            v.cache
                .get(std::path::Path::new("a.png"))
                .unwrap()
                .texture()
                .is_none()
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
