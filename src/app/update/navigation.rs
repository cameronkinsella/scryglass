use std::path::PathBuf;
use std::sync::Arc;

use iced::Task;
use iced::time::Instant;

use crate::anim::AnimPlayer;
use crate::app::state::{Direction, Session, Viewer};
use crate::app::{App, MediaMessage, Message, OpenMessage};
use crate::config::ZoomMode;
use crate::media::archive::{self, ArchiveIndex};
use crate::media::pipeline::{Lane, Source, ThumbUrgency};
use crate::nav::{self, Nav};

use super::NavTarget;
use super::media_tasks::{
    fire_exif, fire_load, fire_prefetch, fire_thumb, fire_thumbnailer, fire_visible_thumbs,
    probe_size, show_loaded, show_placeholder_or_clear,
};
use super::video_flow::start_video;

/// Re-sort the open folder by the configured key off-thread. Metadata
/// (date/size) is fetched only when the key needs it, archives use their
/// index and never touch the filesystem.
pub(crate) fn fire_resort(app: &App) -> Task<Message> {
    let Some(viewer) = app.viewer() else {
        return Task::none();
    };
    let key = app.config.sort_key;
    let desc = app.config.sort_desc;
    let files = viewer.nav.files().to_vec();
    let source = viewer.source.clone();

    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let needs_meta = matches!(
                    key,
                    crate::config::SortKey::DateModified | crate::config::SortKey::Size
                );
                let entries: Vec<nav::FileMeta> = files
                    .into_iter()
                    .map(|path| {
                        let (modified, size) = match (&source, needs_meta) {
                            (Source::Fs, true) => {
                                let meta = std::fs::metadata(&path).ok();
                                (
                                    meta.as_ref().and_then(|m| m.modified().ok()),
                                    meta.map(|m| m.len()).unwrap_or(0),
                                )
                            }
                            (Source::Archive(index), true) => {
                                (None, index.entry_size(&path).unwrap_or(0))
                            }
                            _ => (None, 0),
                        };
                        nav::FileMeta {
                            path,
                            modified,
                            size,
                        }
                    })
                    .collect();
                nav::sort_paths(entries, key, desc)
            })
            .await
            .unwrap_or_default()
        },
        |files| Message::Media(MediaMessage::Resorted(files)),
    )
}

/// Build a fresh viewer over `nav` and fire the initial loads.
pub(crate) fn open_viewer(
    app: &mut App,
    nav: Nav,
    source: Source,
    opened_container: bool,
) -> Task<Message> {
    let depth = app.config.prefetch_depth;
    let budget = app.config.cache_budget_mb * 1024 * 1024;
    let window_w = app.window_size.width;
    let pipeline = app.pipeline.clone();

    // Privacy hygiene: purge persisted thumbnails of files that were
    // deleted from this folder/archive since the last visit. Uses the
    // listing we already have, with no extra source I/O.
    let reconcile = match pipeline.disk() {
        Some(disk) => {
            let (container, _) = crate::media::pipeline::cache_key(
                &source,
                nav.files()
                    .first()
                    .map_or_else(|| std::path::Path::new(""), |p| p.as_path()),
            );
            let live: Vec<std::ffi::OsString> = nav
                .files()
                .iter()
                .map(|p| match &source {
                    Source::Fs => p.file_name().unwrap_or_default().to_owned(),
                    Source::Archive(_) => p.as_os_str().to_owned(),
                })
                .collect();
            Task::future(async move {
                let _ =
                    tokio::task::spawn_blocking(move || disk.reconcile(&container, &live)).await;
            })
            .discard()
        }
        None => Task::none(),
    };

    let mut viewer = Viewer::new(nav, source, AnimPlayer::new(), budget);
    let current = viewer.nav.current().to_path_buf();
    let mut tasks = vec![reconcile, probe_size(&mut viewer, current.clone())];

    if crate::video::is_video(&current) {
        tasks.push(start_video(
            &mut viewer,
            current,
            app.config.video_volume,
            app.config.video_muted,
            app.config.video_loop,
            app.config.hardware_decode,
        ));
    } else {
        tasks.push(fire_thumb(
            &pipeline,
            &mut viewer,
            current.clone(),
            ThumbUrgency::Urgent,
        ));
        tasks.push(fire_load(&pipeline, &mut viewer, current, Lane::Current));
    }
    tasks.extend(fire_prefetch(&pipeline, &mut viewer, depth));
    tasks.extend(fire_visible_thumbs(&pipeline, &mut viewer, window_w));
    // Background-thumbnail the whole directory so the filmstrip and
    // placeholders are warm everywhere, not just near the cursor.
    tasks.extend(fire_thumbnailer(&pipeline, &mut viewer, 3));

    viewer.resort_to_first = opened_container;
    app.session = Session::Viewing(Box::new(viewer));

    // Folders open in name order instantly. A configured custom sort
    // applies as soon as its metadata is gathered.
    if app.config.sort_key != crate::config::SortKey::Name || app.config.sort_desc {
        tasks.push(fire_resort(app));
    }
    if app.config.show_info {
        tasks.push(fire_exif(app));
    }

    Task::batch(tasks)
}

/// Shared logic for opening a path (from drop, dialog, or CLI argument).
///
/// A file opens at that file within its parent directory. A directory
/// opens at its first supported image. An archive opens at its first
/// image entry.
pub(crate) fn open_path(path: PathBuf) -> Task<Message> {
    if archive::is_archive(&path) {
        return Task::perform(
            async move {
                let result = tokio::task::spawn_blocking({
                    let p = path.clone();
                    move || ArchiveIndex::open(&p).map(Arc::new)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
                (path, result)
            },
            |(path, result)| Message::Open(OpenMessage::ArchiveScanned(path, result)),
        );
    }

    Task::perform(
        async move {
            let (dir, start) = if path.is_dir() {
                (path, None)
            } else {
                let dir = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                (dir, Some(path))
            };

            let opened_dir = start.is_none();
            match nav::scan_directory(&dir) {
                Ok(files) => match start.or_else(|| files.first().cloned()) {
                    Some(start) => (start, opened_dir, Ok(files)),
                    None => (
                        dir,
                        opened_dir,
                        Err(String::from("directory contains no supported images")),
                    ),
                },
                Err(e) => (start.unwrap_or(dir), opened_dir, Err(e.to_string())),
            }
        },
        |(path, opened_dir, result)| {
            Message::Open(OpenMessage::DirectoryScanned(path, opened_dir, result))
        },
    )
}

/// Move the cursor (one step or to an absolute index), then update the
/// display from cache and fire loads. Never waits on anything.
pub(crate) fn navigate(app: &mut App, target: NavTarget) -> Task<Message> {
    let pipeline = app.pipeline.clone();
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };

    // Key navigation while a move is pending is dropped: the screen
    // advances at the rate blurs become available, never out of sync.
    // Absolute jumps (slider, filmstrip) replace the pending target
    // instead. Latest wins.
    if viewer.pending_nav.is_some() && matches!(target, NavTarget::Delta(_)) {
        return Task::none();
    }

    let len = viewer.nav.len();
    let cursor = viewer.nav.cursor();
    let target_index = match target {
        NavTarget::Delta(Direction::Forward) => (cursor + 1) % len,
        NavTarget::Delta(Direction::Backward) => (cursor + len - 1) % len,
        NavTarget::Index(index) => {
            let index = index % len;
            if index == cursor {
                viewer.pending_nav = None;
                return Task::none();
            }
            index
        }
    };
    let target_path = viewer.nav.files()[target_index].to_path_buf();

    // Something is on hand to show (full image, blur, or cached GIF):
    // move immediately.
    if viewer.displayable(&target_path) {
        return complete_navigation(app, target_index, true);
    }

    // Nothing displayable yet. Hold position (current image stays on
    // screen, spinner appears after the grace period) and move the moment
    // the target has at least a blur.
    viewer.pending_nav = Some(target_index);
    viewer.pending_since = Some(Instant::now());

    let tasks = vec![
        fire_thumb(&pipeline, viewer, target_path.clone(), ThumbUrgency::Urgent),
        fire_load(&pipeline, viewer, target_path, Lane::Current),
    ];
    Task::batch(tasks)
}

/// Mid-drag scrub step onto an already-loaded file: move display, title,
/// and cursor together with minimal side effects: no generation bump, no
/// prefetch, no probes, no filmstrip centering. Those run once on release.
pub(crate) fn scrub_to(app: &mut App, index: usize) -> Task<Message> {
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };

    viewer.nav.set_cursor(index);
    viewer.pending_nav = None;
    viewer.anim_player.stop();
    viewer.drag = None;
    viewer.pan = (0.0, 0.0);
    if zoom_mode != ZoomMode::LockZoomRatio {
        viewer.manual_zoom = false;
    }
    viewer.current_file_size = None;
    viewer.exif = None;
    viewer.rotation = 0;
    viewer.displayed_rotation = 0;
    viewer.video = None;
    viewer.video_frame = None;
    viewer.video_seek_drag = None;
    viewer.video_extracting = None;

    let current = viewer.nav.current().to_path_buf();
    if let Some(cached) = viewer.cache.get(&current).cloned() {
        show_loaded(viewer, &current, cached, zoom_mode, viewport);
    } else {
        // Scrub targets are guaranteed at least a thumb. The sharp image
        // loads if the drag ends here.
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
    }
    Task::none()
}

/// A pending navigation's target just became displayable, finish the move.
pub(crate) fn resolve_pending_nav(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let Some(target_index) = viewer.pending_nav else {
        return Task::none();
    };
    let target_path = viewer.nav.files()[target_index].to_path_buf();
    if viewer.displayable(&target_path) {
        // No generation bump: the in-flight load for this very image must
        // survive the move (a bump would cancel it and double the decode).
        complete_navigation(app, target_index, false)
    } else {
        Task::none()
    }
}

/// Move the cursor to `target_index`, which must have something
/// displayable, then update display, prefetch, caches, and filmstrip.
pub(crate) fn complete_navigation(
    app: &mut App,
    target_index: usize,
    bump_generation: bool,
) -> Task<Message> {
    let depth = app.config.prefetch_depth;
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let window_w = app.window_size.width;
    let show_filmstrip = app.config.show_filmstrip;
    let video_volume = app.config.video_volume;
    let video_muted = app.config.video_muted;
    let video_loop = app.config.video_loop;
    let hardware = app.config.hardware_decode;
    let pipeline = app.pipeline.clone();
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };

    viewer.nav.set_cursor(target_index);
    viewer.pending_nav = None;

    if bump_generation {
        // Everything in flight for the old position is now stale.
        pipeline.bump_generation();
    }

    viewer.anim_player.stop();
    viewer.video = None;
    viewer.video_frame = None;
    viewer.video_seek_drag = None;
    viewer.video_extracting = None;
    viewer.drag = None;

    // Reset pan on navigation. Zoom is preserved only in LockZoomRatio mode.
    viewer.pan = (0.0, 0.0);
    if zoom_mode != ZoomMode::LockZoomRatio {
        viewer.manual_zoom = false;
    }

    viewer.current_file_size = None;
    viewer.rotation = 0;
    viewer.displayed_rotation = 0;

    // The animation decode cache prunes by window, the image cache by
    // byte budget.
    let keep = viewer.pinned_paths(depth);
    viewer.anim_player.prune_cache(&keep);

    let current = viewer.nav.current().to_path_buf();
    let mut tasks = vec![probe_size(viewer, current.clone())];

    if crate::video::is_video(&current) {
        // Video: blur-free spinner until the first frame arrives. The
        // session's tick subscription drives frames from here.
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
        tasks.push(start_video(
            viewer,
            current.clone(),
            video_volume,
            video_muted,
            video_loop,
            hardware,
        ));
    } else if let Some(anim_task) = viewer.anim_player.try_start_from_cache(&current) {
        // Cached animation: blur stands in until frame 0 allocates.
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
        tasks.push(anim_task.map(Message::Anim));
    } else if let Some(cached) = viewer.cache.get(&current).cloned() {
        // Instant display, the common case within the prefetch window.
        show_loaded(viewer, &current, cached, zoom_mode, viewport);
    } else {
        // Navigation only lands on displayable targets, so the blur is
        // guaranteed here. The full image (or animation) loads behind it.
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
        tasks.push(fire_load(&pipeline, viewer, current, Lane::Current));
    }

    tasks.extend(fire_prefetch(&pipeline, viewer, depth));

    let pinned = viewer.pinned_paths(depth);
    viewer.cache.evict_over_budget(&pinned);

    if show_filmstrip {
        // Keep the filmstrip centered on the cursor and its thumbs warm.
        let center = crate::components::filmstrip::centering_offset(viewer.nav.cursor(), window_w);
        viewer.filmstrip_scroll_x = center;
        tasks.push(iced::widget::operation::scroll_to(
            crate::components::filmstrip::filmstrip_id(),
            iced::widget::scrollable::AbsoluteOffset { x: center, y: 0.0 },
        ));
        tasks.extend(fire_visible_thumbs(&pipeline, viewer, window_w));
    }

    if app.config.show_info {
        tasks.push(fire_exif(app));
    }

    Task::batch(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn a_step_is_dropped_while_a_move_is_pending() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = navigate(&mut app, NavTarget::Delta(Direction::Forward));
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 0); // never moved
        assert_eq!(v.pending_nav, Some(1)); // pending target untouched
    }

    #[test]
    fn an_absolute_jump_replaces_a_pending_target() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = navigate(&mut app, NavTarget::Index(2));
        // c.png isn't cached, so the pending target moves to it (latest wins).
        assert_eq!(app.viewer().unwrap().pending_nav, Some(2));
    }

    #[test]
    fn jumping_to_the_current_index_clears_the_pending_move() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = navigate(&mut app, NavTarget::Index(0));
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 0);
        assert!(v.pending_nav.is_none());
    }

    #[test]
    fn a_cache_miss_holds_position_and_defers_the_move() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = navigate(&mut app, NavTarget::Delta(Direction::Forward));
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 0); // stays put, screen never goes empty
        assert_eq!(v.pending_nav, Some(1));
    }

    #[test]
    fn resolve_pending_waits_until_the_target_is_displayable() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        let _ = resolve_pending_nav(&mut app);
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 0);
        assert_eq!(v.pending_nav, Some(1)); // still nothing to show
    }

    #[test]
    fn a_step_moves_at_once_when_the_target_has_a_blur() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        crate::app::test_support::cache_thumb(&mut app, "b.png", 4, 4);
        let _ = navigate(&mut app, NavTarget::Delta(Direction::Forward));
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 1); // b.png had a thumb, so we moved
        assert!(v.pending_nav.is_none());
    }

    #[test]
    fn resolve_pending_completes_once_a_blur_arrives() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().pending_nav = Some(1);
        crate::app::test_support::cache_thumb(&mut app, "b.png", 4, 4);
        let _ = resolve_pending_nav(&mut app);
        let v = app.viewer().unwrap();
        assert_eq!(v.nav.cursor(), 1);
        assert!(v.pending_nav.is_none());
    }
}
