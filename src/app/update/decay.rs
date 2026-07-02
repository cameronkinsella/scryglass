//! Resource decay for backgrounded windows: ordered stages demote, drop,
//! and evict the on-screen media, shed the prefetch look-ahead, and
//! restore everything when the window returns.

use std::time::Duration;

use iced::{Size, Task};

use super::window::Message;
use super::{fire_load, fire_prefetch, fire_rotate, run_jobs_at, try_start_shared_anim};
use crate::app::state::{DisplayedImage, Viewer};
use crate::app::{Message as AppMessage, Shared, Window};
use crate::config::{
    EvictConfig, PrefetchDecay, PrefetchDropAnchor, PrefetchVram, StateDecayRef, VideoDecay,
};
use crate::media::pipeline::{Lane, Pipeline};
use crate::media::store::{ImageKey, Tier};

/// One stage of a backgrounded window's decay pipeline, run in this order
/// and each at its own delay from when the window entered the state. The same
/// stages serve both the unfocused and minimized states. Only their configured
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
    /// Release an open video's whole decode session, freezing the last frame. A
    /// restore re-opens it at the saved position.
    EvictVideo,
    /// Release the furthest remaining ring of prefetched neighbors, then re-arm
    /// itself after the configured interval while any remain. Started by its
    /// configured anchor event (state entry or one of the stages above), so it
    /// runs beside the on-screen pipeline, not inside it.
    ShedPrefetch,
}

/// Run one decay stage, unless a focus or minimize change has since bumped
/// the generation it was armed under (then it is stale and no-ops).
pub(super) fn run_decay_stage(
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
        let interval = shared
            .config
            .resource
            .for_state(win.minimized)
            .prefetch
            .drop_interval;
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
            // Capture the session into a memo (keeps the archive temp file alive) and
            // drop it, freeing decode threads, hardware decoder, audio sink, and GPU
            // textures. The last `frame` stays on screen, so the video looks paused,
            // not gone. `playing` resolves through the minimize-pause flag, which may
            // have already cleared `session.playing`.
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
    // texture the store does not govern) is pure waste. A restore re-derives
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
            // Drop the texture. The view falls back to the thumbnail blur, and a
            // refocus re-uploads from the RAM source with no disk read.
            retarget_displayed(viewer, shared, Tier::InRam, &pipeline, view)
        }
        DecayStage::EvictRam => {
            let is_anim = matches!(viewer.displayed, DisplayedImage::Animated { .. });
            if is_anim {
                // Lower this window's demand on the shared frames to evicted, exactly
                // like a still lowering its tier. The lease and dormant playback are
                // kept, so aggregate demand alone decides residency: the frames free
                // only once every window drops to evicted, and any window bringing
                // them back resumes every playback. Decay is shared, like stills.
                if let Some(path) = viewer.displayed_path.clone()
                    && let Some(lease) = viewer.anim_player.lease(&path)
                {
                    shared.anim_store.retarget(lease, Tier::Evicted);
                }
                Task::none()
            } else {
                // A still lowers its demand to evicted: the store frees its RAM once
                // no window wants it higher. While another window holds the image it
                // stays sharp. Once the last holder releases it the cell empties and
                // the view falls back to the blur. A return re-decodes.
                retarget_displayed(viewer, shared, Tier::Evicted, &pipeline, view)
            }
        }
        DecayStage::EvictVideo | DecayStage::ShedPrefetch => {
            unreachable!("handled before the viewer borrow above")
        }
    }
}

/// When the prefetch shedding starts, measured from state entry: the earliest
/// armed stage at or after the anchor in pipeline order, plus the configured
/// delay. A skipped anchor falls through to the next stage that runs, so
/// `drop_on = "demote"` still sheds when only the drop stage is enabled.
/// `None` (keep the prefetch) when nothing at or after the anchor runs.
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
pub(super) fn restart_decay(win: &mut Window, shared: &mut Shared) -> Task<AppMessage> {
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
    let media = decay_media(win);

    // The eviction delay scales with the on-screen image's decode time, measured at
    // decode by whichever store owns it (the animation store for a GIF). A video has
    // no decode-time-scaled timer, so this is unused for it.
    let decode = win.viewer().and_then(|v| {
        let path = v.displayed_path.as_deref()?;
        let key = ImageKey::new(&v.source, path);
        if media == DecayMedia::Animated {
            shared.anim_store.decode_time(&key)
        } else {
            shared.store.decode_time(&key)
        }
    });

    let state = shared.config.resource.for_state(win.minimized);
    let stages = schedule_for(media, state, decode);
    // The shedding's start is resolved against the stages that actually run
    // (a skipped anchor falls through), so it is armed here as one absolute
    // timer from state entry, like the stages themselves.
    if let Some(delay) = shed_start(state.prefetch, &stages) {
        tasks.push(arm_decay(generation, DecayStage::ShedPrefetch, delay));
    }
    for (stage, delay) in stages {
        tasks.push(arm_decay(generation, stage, delay));
    }
    Task::batch(tasks)
}

/// Which decay pipeline the on-screen media runs. The kinds are mutually
/// exclusive on screen: a video releases its whole decode session, an animation
/// evicts its RAM frames, a still runs the full demote/drop/evict pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DecayMedia {
    Still,
    Animated,
    Video,
}

/// The media kind whose decay pipeline governs the window right now.
fn decay_media(win: &Window) -> DecayMedia {
    let Some(viewer) = win.viewer() else {
        return DecayMedia::Still;
    };
    if viewer.video.session.is_some() || viewer.video.suspended.is_some() {
        DecayMedia::Video
    } else if matches!(viewer.displayed, DisplayedImage::Animated { .. }) {
        DecayMedia::Animated
    } else {
        DecayMedia::Still
    }
}

/// The stages the given media kind arms in the given state.
fn schedule_for(
    media: DecayMedia,
    state: StateDecayRef<'_>,
    decode: Option<Duration>,
) -> Vec<(DecayStage, Duration)> {
    match media {
        DecayMedia::Video => video_decay_schedule(state.video),
        DecayMedia::Animated => anim_decay_schedule(state.animated, decode),
        DecayMedia::Still => decay_schedule(state.still, decode),
    }
}

/// The evict stage and its delay for an animation, given its `.animated` config.
/// An animation has no VRAM tier, so eviction is the only stage it can ever run.
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
/// display stays put. The view re-sharpens once the texture lands. A minimized
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
    // tick (re-armed once the session exists) paints the fresh one over it. The first
    // poll delivers it even while paused, so a paused video shows the right frame.
    if let Some(memo) = viewer.video.suspended.take() {
        viewer.video.session = Some(crate::video::VideoSession::resume(&memo));
    }
    if let Some(displayed) = viewer.displayed_path.clone() {
        let is_anim = matches!(viewer.displayed, DisplayedImage::Animated { .. });
        if is_anim {
            // Re-lease the shared frames if any window still has them resident (no
            // decode). Otherwise re-decode through the still path, which re-discovers
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
    // A focused window re-warms its look-ahead. A window reactivated by a
    // scroll restores only the visible image.
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
    use crate::app::test_support::cache_image;
    use crate::app::update::window::update;

    #[test]
    fn schedule_for_dispatches_by_media_kind() {
        let res = crate::config::ResourceConfig::default();
        let min = res.for_state(true);

        let video = schedule_for(DecayMedia::Video, min, None);
        assert!(matches!(video.as_slice(), [(DecayStage::EvictVideo, _)]));

        let anim = schedule_for(DecayMedia::Animated, min, Some(Duration::ZERO));
        assert!(matches!(anim.as_slice(), [(DecayStage::EvictRam, _)]));

        let still = schedule_for(DecayMedia::Still, min, Some(Duration::ZERO));
        assert!(still.iter().any(|(s, _)| *s == DecayStage::DropVram));
        assert!(still.iter().all(|(s, _)| *s != DecayStage::EvictVideo));
    }

    #[test]
    fn minimizing_drops_the_texture_but_keeps_the_ram() {
        use crate::app::test_support::viewing_app;

        let mut app = viewing_app(&["a.png"], 0);
        cache_image(&mut app, "a.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

        // Minimizing arms the drop-VRAM stage (at 0s by default). Fire it.
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

        // The image stays leased with its RAM in the store. Only the shared
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

        // The override's texture is freed. The wanted rotation survives so a
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
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Demote), &full),
            Some(s(13))
        );
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Drop), &full),
            Some(s(23))
        );
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Evict), &full),
            Some(s(33))
        );

        // A skipped anchor falls through to the next stage that runs: demote
        // disabled, drop at 0s (the default minimized pipeline).
        let drop_only = [(DecayStage::DropVram, Duration::ZERO)];
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Demote), &drop_only),
            Some(s(3))
        );

        // Nothing at or after the anchor runs: the prefetch is kept.
        let demote_only = [(DecayStage::Demote, s(15))];
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Evict), &demote_only),
            None
        );
        assert_eq!(shed_start(&cfg(PrefetchDropAnchor::Demote), &[]), None);

        // A video's session release stands in for its evict, and earlier
        // anchors fall through to it.
        let video = [(DecayStage::EvictVideo, s(5))];
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Demote), &video),
            Some(s(8))
        );
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Evict), &video),
            Some(s(8))
        );

        // Immediately counts from state entry, whatever the pipeline runs.
        assert_eq!(
            shed_start(&cfg(PrefetchDropAnchor::Immediately), &[]),
            Some(s(3))
        );
    }

    #[test]
    fn a_stale_idle_timer_keeps_the_prefetch() {
        use crate::app::test_support::viewing_app;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        cache_image(&mut app, "a.png");
        cache_image(&mut app, "b.png");
        app.viewer_mut().unwrap().displayed_path = Some("a.png".into());

        let _ = update(&mut app.window, &mut app.shared, Message::Focused(false));
        // A refocus supersedes the drop. The old timer firing late must no-op.
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

        // Demote plus a fixed evict. The disabled ("never") drop is skipped, and
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

        // Dynamic eviction needs a measured decode time. Without one it never
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

        // "never" arms nothing, so a backgrounded animation keeps its frames.
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
    // FFmpeg threads). The wiring it tests is identical in both builds.
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
}
