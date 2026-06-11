//! Update function: handles messages, mutates state, fires async tasks.
//!
//! Navigation NEVER blocks: every keypress moves the cursor immediately.
//! A cache hit displays instantly. A miss keeps the previous image on
//! screen and fires a cancellable load. Whatever load finishes for the
//! path under the cursor wins ("latest wins" by path equality).

use std::path::PathBuf;

use iced::time::Instant;
use iced::widget::image::Handle;
use iced::{Size, Task};

use std::sync::Arc;

use crate::cache;
use crate::config::{AppConfig, ThemeChoice, ZoomMode};
use crate::gif::{self, GifPlayer};
use crate::media::archive::{self, ArchiveIndex};
use crate::media::pipeline::{Lane, Pipeline, Source, ThumbUrgency};
use crate::media::registry::DecodeOpts;
use crate::media::{DecodedMedia, MediaError};
use crate::nav::{self, Nav};
use crate::ui;
use crate::ui::toast::{Toast, ToastKind};
use crate::ui::toolbar::OpenMenu;

use super::message::{is_context_menu_message, is_menu_message};
use super::state::{
    CachedImage, Direction, DisplayedImage, DragState, LoadedMedia, Session, Thumb, Viewer,
};
use super::viewer_math::{clamp_pan, compute_zoom, pan_for_zoom_toward_cursor};
use super::{App, Message, TOOLBAR_HEIGHT, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP, recalc_viewport};

/// Where a navigation lands: one step in a direction, or an absolute index.
enum NavTarget {
    Delta(Direction),
    Index(usize),
}

/// Update function: handles messages and mutates state.
pub fn update(app: &mut App, message: Message) -> Task<Message> {
    // Auto-dismiss any open dropdown when the user interacts outside the menu.
    if app.open_menu.is_some() && !is_menu_message(&message) {
        app.open_menu = None;
    }

    // Auto-dismiss context menu on any non-context-menu interaction.
    if app.context_menu_pos.is_some() && !is_context_menu_message(&message) {
        app.context_menu_pos = None;
    }

    match message {
        Message::FileDropped(path) => open_path(path),

        Message::DirectoryScanned(start_file, Ok(files)) => match Nav::new(files, &start_file) {
            Ok(nav) => open_viewer(app, nav, Source::Fs),
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't open: {e}")),
        },

        Message::DirectoryScanned(_start_file, Err(err)) => {
            push_toast(app, ToastKind::Error, format!("Couldn't open: {err}"))
        }

        Message::ArchiveScanned(archive_path, Ok(index)) => {
            let entries = index.image_entries();
            let start = entries.first().cloned();
            match start.and_then(|s| Nav::new(entries, &s).ok()) {
                Some(nav) => open_viewer(app, nav, Source::Archive(index)),
                None => push_toast(
                    app,
                    ToastKind::Error,
                    format!(
                        "{}: archive contains no supported images",
                        archive_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                ),
            }
        }

        Message::ArchiveScanned(_archive_path, Err(err)) => push_toast(
            app,
            ToastKind::Error,
            format!("Couldn't open archive: {err}"),
        ),

        Message::MediaLoaded { path, result } => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let depth = app.config.prefetch_depth;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.in_flight.remove(&path);

            match result {
                Ok(loaded) => {
                    // A "stale" decode is a free prefetch, so cache it anyway.
                    let image = loaded.image;
                    viewer
                        .cache
                        .insert(path.clone(), image.clone(), image.byte_cost());
                    if let Some(thumb) = loaded.thumb {
                        let cost = thumb.byte_cost();
                        viewer.thumbs.insert(path.clone(), thumb, cost);
                    }
                    if viewer.nav.current() == path {
                        show_loaded(viewer, &path, image, zoom_mode, viewport);
                    }
                    let pinned = viewer.pinned_paths(depth);
                    viewer.cache.evict_over_budget(&pinned);
                    viewer.thumbs.evict_over_budget(&pinned);
                    resolve_pending_nav(app)
                }
                Err(MediaError::Cancelled) => {
                    // Cancelled loads that are still wanted get re-fired with
                    // the live generation.
                    let pending_path = viewer
                        .pending_nav
                        .map(|i| viewer.nav.files()[i].to_path_buf());
                    if viewer.nav.current() == path || pending_path.as_deref() == Some(&*path) {
                        fire_load(&pipeline, viewer, path, Lane::Current)
                    } else if viewer.pinned_paths(depth).contains(&path) {
                        fire_load(&pipeline, viewer, path, Lane::Prefetch)
                    } else {
                        Task::none()
                    }
                }
                Err(err) => {
                    let pending_path = viewer
                        .pending_nav
                        .map(|i| viewer.nav.files()[i].to_path_buf());
                    let is_current = viewer.nav.current() == path;
                    let is_pending = pending_path.as_deref() == Some(&*path);
                    if !is_current && !is_pending {
                        return Task::none();
                    }
                    // Stop the spinner, a failed pending move stays put.
                    viewer.pending_since = None;
                    if is_pending {
                        viewer.pending_nav = None;
                    }
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    push_toast(app, ToastKind::Error, format!("{name}: {err}"))
                }
            }
        }

        Message::ThumbLoaded {
            path,
            urgency,
            result,
        } => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.in_flight_thumbs.remove(&path);

            match result {
                Ok(thumb) => {
                    let cost = thumb.byte_cost();
                    viewer.thumbs.insert(path.clone(), thumb.clone(), cost);
                    // Show as placeholder if this is the image being waited on.
                    if viewer.nav.current() == path
                        && viewer.pending_since.is_some()
                        && viewer.pending_nav.is_none()
                    {
                        show_placeholder(viewer, &path, thumb, zoom_mode, viewport);
                    }
                }
                Err(_) => {
                    // An urgent probe finding no EXIF preview is normal. The
                    // background fallback will still thumbnail the file. Only
                    // a failed background attempt writes the file off.
                    if urgency == ThumbUrgency::Background {
                        viewer.failed_thumbs.insert(path.clone());
                    }
                }
            }

            // Keep the background thumbnailer chain going, and complete a
            // pending move if this thumb was what it waited for.
            let mut tasks = fire_thumbnailer(&pipeline, viewer, 1);
            tasks.push(resolve_pending_nav(app));
            Task::batch(tasks)
        }

        Message::FileSizeProbed(path, size) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.current_file_size = Some(size);
            }
            Task::none()
        }

        // Forces a redraw so the spinner animates (angle derives from time).
        Message::SpinnerTick => Task::none(),

        Message::DismissToast(id) => {
            app.toasts.retain(|t| t.id != id);
            Task::none()
        }

        Message::Gif(gif_msg) => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            let is_first_frame = matches!(viewer.displayed, DisplayedImage::None)
                || (viewer.pending_since.is_some()
                    && matches!(&gif_msg, gif::GifMessage::FrameAllocated(..)));

            let (task, allocation) = viewer.gif_player.update(gif_msg, viewer.nav.current());

            if let Some(alloc) = allocation {
                let size = alloc.size();
                if is_first_frame && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
                    viewer.zoom = compute_zoom(zoom_mode, size.width, size.height, viewport);
                    viewer.pan = (0.0, 0.0);
                }
                viewer.displayed = DisplayedImage::Full {
                    allocation: alloc,
                    original_size: (size.width, size.height),
                };
                viewer.displayed_path = Some(viewer.nav.current().to_path_buf());
                viewer.pending_since = None;
            }

            // A pending move onto a GIF resolves once its decode lands.
            Task::batch([task.map(Message::Gif), resolve_pending_nav(app)])
        }

        // --- Initial press: always navigate + record hold start ---
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

        // --- OS key-repeat: only navigate if held past threshold ---
        Message::NextRepeat => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let past = viewer
                .held_direction
                .map(|(_, t)| t.elapsed() >= super::HOLD_THRESHOLD)
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
                .map(|(_, t)| t.elapsed() >= super::HOLD_THRESHOLD)
                .unwrap_or(false);
            if !past {
                return Task::none();
            }
            navigate(app, NavTarget::Delta(Direction::Backward))
        }

        // --- Key released: stop continuous scrolling ---
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

        // --- Menu state ---
        Message::ToggleFileMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::File) {
                None
            } else {
                Some(OpenMenu::File)
            };
            Task::none()
        }

        Message::ToggleZoomMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Zoom) {
                None
            } else {
                Some(OpenMenu::Zoom)
            };
            Task::none()
        }

        Message::ToggleLayoutMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Layout) {
                None
            } else {
                Some(OpenMenu::Layout)
            };
            Task::none()
        }

        Message::DismissOverlay => {
            app.open_menu = None;
            Task::none()
        }

        // --- File menu actions ---
        Message::OpenFile => {
            app.open_menu = None;
            let extensions = AppConfig::supported_extensions()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            Task::perform(
                async move {
                    let handle = rfd::AsyncFileDialog::new()
                        .add_filter(
                            "Images",
                            &extensions.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        )
                        .add_filter(
                            "Archives",
                            &["zip", "cbz", "tar", "gz", "tgz", "7z", "cb7", "rar", "cbr"],
                        )
                        .pick_file()
                        .await;
                    handle.map(|h| h.path().to_path_buf())
                },
                Message::FileDialogResult,
            )
        }

        Message::FileDialogResult(Some(path)) => open_path(path),
        Message::FileDialogResult(None) => Task::none(),

        Message::CloseFile => {
            app.open_menu = None;
            app.session = Session::Empty;
            Task::none()
        }

        Message::Quit => iced::exit(),

        // --- Zoom mode ---
        Message::SetZoomMode(mode) => {
            app.open_menu = None;
            app.config.zoom_mode = mode;
            let viewport = app.viewport_size;

            if let Some(viewer) = app.viewer_mut() {
                viewer.manual_zoom = false;

                if let Some((w, h)) = viewer.displayed.original_size() {
                    viewer.zoom = compute_zoom(mode, w, h, viewport);
                    let img_w = w as f32 * viewer.zoom;
                    let img_h = h as f32 * viewer.zoom;
                    viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
                }
            }
            save_config(app)
        }

        // --- Scroll-wheel zoom (toward cursor) ---
        Message::ScrollZoom(delta_y) => {
            let viewport = app.viewport_size;
            let cursor = app.last_cursor_pos;
            let toolbar_offset = if app.config.show_toolbar {
                TOOLBAR_HEIGHT
            } else {
                0.0
            };

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            let old_zoom = viewer.zoom;
            let factor = if delta_y > 0.0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            viewer.zoom = (old_zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            viewer.manual_zoom = true;

            // Adjust pan so the source pixel under the cursor stays fixed.
            // The cursor offset is measured from the viewport center
            // (window cursor pos minus toolbar height).
            let ratio = viewer.zoom / old_zoom;
            let d = (
                cursor.x - viewport.width / 2.0,
                cursor.y - toolbar_offset - viewport.height / 2.0,
            );
            viewer.pan = pan_for_zoom_toward_cursor(viewer.pan, ratio, d);

            if let Some((w, h)) = viewer.displayed.original_size() {
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        // --- Double-click: reset zoom ---
        Message::ResetZoom => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.manual_zoom = false;
            if let Some((w, h)) = viewer.displayed.original_size() {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }
            viewer.pan = (0.0, 0.0);
            Task::none()
        }

        // --- Drag to pan ---
        Message::DragStart => {
            let cursor = app.last_cursor_pos;
            if let Some(viewer) = app.viewer_mut() {
                viewer.drag = Some(DragState {
                    start: cursor,
                    start_pan: viewer.pan,
                });
            }
            Task::none()
        }

        Message::DragMove(pos) => {
            app.last_cursor_pos = pos;
            let viewport = app.viewport_size;

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

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
            Task::none()
        }

        Message::DragEnd => {
            if let Some(viewer) = app.viewer_mut() {
                viewer.drag = None;
            }
            Task::none()
        }

        // --- Window resized ---
        Message::WindowResized(size) => {
            app.window_size = size;
            recalc_viewport(app);
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            if let Some((w, h)) = viewer.displayed.original_size() {
                if !viewer.manual_zoom {
                    viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
                }
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        // --- Slider and filmstrip visibility ---
        Message::ToggleFilmstrip => {
            app.config.show_filmstrip = !app.config.show_filmstrip;
            recalc_viewport(app);
            save_config(app)
        }

        Message::ToggleSlider => {
            app.config.show_slider = !app.config.show_slider;
            recalc_viewport(app);
            save_config(app)
        }

        Message::ToggleFooter => {
            app.config.show_footer = !app.config.show_footer;
            recalc_viewport(app);
            save_config(app)
        }

        Message::FilmstripScroll(delta_y) => {
            // Convert vertical scroll delta to horizontal scroll on the filmstrip.
            let offset = iced::widget::scrollable::AbsoluteOffset {
                x: -delta_y * 60.0,
                y: 0.0,
            };
            iced::widget::operation::scroll_by(ui::filmstrip::filmstrip_id(), offset)
        }

        Message::FilmstripScrolled(x) => {
            let window_w = app.window_size.width;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.filmstrip_scroll_x = x;
            Task::batch(fire_visible_thumbs(&pipeline, viewer, window_w))
        }

        Message::SliderChanged(index) | Message::FilmstripClicked(index) => {
            navigate(app, NavTarget::Index(index))
        }

        // --- Toolbar visibility ---
        Message::ToggleToolbar => {
            app.config.show_toolbar = !app.config.show_toolbar;
            app.context_menu_pos = None;
            recalc_viewport(app);
            save_config(app)
        }

        Message::ToggleCrispPixels => {
            app.config.crisp_pixels = !app.config.crisp_pixels;
            save_config(app)
        }

        // --- Theme ---
        Message::ToggleTheme => {
            app.config.theme = match app.config.theme {
                ThemeChoice::Dark => ThemeChoice::Light,
                ThemeChoice::Light => ThemeChoice::Dark,
            };
            save_config(app)
        }

        // --- Context menu ---
        Message::ShowContextMenu => {
            app.context_menu_pos = Some(app.last_cursor_pos);
            Task::none()
        }

        Message::DismissContextMenu => {
            app.context_menu_pos = None;
            Task::none()
        }

        Message::CopyImage => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path = viewer.current_disk_path();
            let copy = Task::future(async move {
                crate::platform::copy_image_to_clipboard(&path);
            })
            .discard();
            Task::batch([
                copy,
                push_toast(app, ToastKind::Info, "Image copied".to_string()),
            ])
        }

        Message::CopyFilePath => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path_str = match &viewer.source {
                Source::Fs => viewer.nav.current().to_string_lossy().to_string(),
                Source::Archive(index) => format!(
                    "{}/{}",
                    index.archive_path.display(),
                    viewer.nav.current().display()
                ),
            };
            Task::batch([
                iced::clipboard::write(path_str),
                push_toast(app, ToastKind::Info, "Path copied".to_string()),
            ])
        }

        Message::CopyFilename => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let name = viewer
                .nav
                .current()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            Task::batch([
                iced::clipboard::write(name),
                push_toast(app, ToastKind::Info, "Filename copied".to_string()),
            ])
        }

        Message::OpenImageLocation => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::reveal_in_file_manager(&viewer.current_disk_path());
            Task::none()
        }

        Message::ImageProperties => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::show_properties(&viewer.current_disk_path());
            Task::none()
        }
    }
}

/// Persist the current config in the background. Saving is fire-and-forget:
/// the viewer must never wait on it.
fn save_config(app: &App) -> Task<Message> {
    Task::future(app.config.clone().save()).discard()
}

/// Show a transient notification that dismisses itself after a few seconds.
fn push_toast(app: &mut App, kind: ToastKind, text: String) -> Task<Message> {
    let id = app.next_toast_id;
    app.next_toast_id += 1;
    app.toasts.push(Toast { id, kind, text });
    Task::perform(
        tokio::time::sleep(std::time::Duration::from_secs(4)),
        move |_| Message::DismissToast(id),
    )
}

/// Build a fresh viewer over `nav` and fire the initial loads.
fn open_viewer(app: &mut App, nav: Nav, source: Source) -> Task<Message> {
    let depth = app.config.prefetch_depth;
    let budget = app.config.cache_budget_mb * 1024 * 1024;
    let window_w = app.window_size.width;
    let pipeline = app.pipeline.clone();

    let mut viewer = Viewer::new(nav, source, GifPlayer::new(), budget);
    let current = viewer.nav.current().to_path_buf();
    let mut tasks = vec![probe_size(&mut viewer, current.clone())];

    if viewer.is_fs() && gif::is_gif(&current) {
        tasks.push(viewer.gif_player.decode_current(&current).map(Message::Gif));
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

    app.session = Session::Viewing(Box::new(viewer));
    Task::batch(tasks)
}

/// Shared logic for opening a path (from drop, dialog, or CLI argument).
///
/// A file opens at that file within its parent directory. A directory
/// opens at its first supported image. An archive opens at its first
/// image entry.
pub fn open_path(path: PathBuf) -> Task<Message> {
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
            |(path, result)| Message::ArchiveScanned(path, result),
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

            match nav::scan_directory(&dir) {
                Ok(files) => match start.or_else(|| files.first().cloned()) {
                    Some(start) => (start, Ok(files)),
                    None => (
                        dir,
                        Err(String::from("directory contains no supported images")),
                    ),
                },
                Err(e) => (start.unwrap_or(dir), Err(e.to_string())),
            }
        },
        |(path, result)| Message::DirectoryScanned(path, result),
    )
}

/// Move the cursor (one step or to an absolute index), then update the
/// display from cache and fire loads. Never waits on anything.
fn navigate(app: &mut App, target: NavTarget) -> Task<Message> {
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

    let mut tasks = Vec::new();
    if viewer.is_fs() && gif::is_gif(&target_path) {
        tasks.push(
            viewer
                .gif_player
                .prefetch_decode(&target_path)
                .map(Message::Gif),
        );
    } else {
        tasks.push(fire_thumb(
            &pipeline,
            viewer,
            target_path.clone(),
            ThumbUrgency::Urgent,
        ));
        tasks.push(fire_load(&pipeline, viewer, target_path, Lane::Current));
    }
    Task::batch(tasks)
}

/// A pending navigation's target just became displayable, finish the move.
fn resolve_pending_nav(app: &mut App) -> Task<Message> {
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
fn complete_navigation(app: &mut App, target_index: usize, bump_generation: bool) -> Task<Message> {
    let depth = app.config.prefetch_depth;
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let window_w = app.window_size.width;
    let show_filmstrip = app.config.show_filmstrip;
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

    viewer.gif_player.stop();
    viewer.drag = None;

    // Reset pan on navigation. Zoom is preserved only in LockZoomRatio mode.
    viewer.pan = (0.0, 0.0);
    if zoom_mode != ZoomMode::LockZoomRatio {
        viewer.manual_zoom = false;
    }

    viewer.current_file_size = None;

    // The GIF decode cache prunes by window, the image cache by byte budget.
    let keep = viewer.pinned_paths(depth);
    viewer.gif_player.prune_cache(&keep);

    let current = viewer.nav.current().to_path_buf();
    let mut tasks = vec![probe_size(viewer, current.clone())];

    if viewer.is_fs() && gif::is_gif(&current) {
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
        if let Some(gif_task) = viewer.gif_player.try_start_from_cache(&current) {
            tasks.push(gif_task.map(Message::Gif));
        } else {
            tasks.push(viewer.gif_player.decode_current(&current).map(Message::Gif));
        }
    } else if let Some(cached) = viewer.cache.get(&current).cloned() {
        // Instant display, the common case within the prefetch window.
        show_loaded(viewer, &current, cached, zoom_mode, viewport);
    } else {
        // Navigation only lands on displayable targets, so the blur is
        // guaranteed here. The full image is loading behind it.
        viewer.pending_since = Some(Instant::now());
        show_placeholder_or_clear(viewer, &current, zoom_mode, viewport);
        tasks.push(fire_load(&pipeline, viewer, current, Lane::Current));
    }

    tasks.extend(fire_prefetch(&pipeline, viewer, depth));

    let pinned = viewer.pinned_paths(depth);
    viewer.cache.evict_over_budget(&pinned);

    if show_filmstrip {
        // Keep the filmstrip centered on the cursor and its thumbs warm.
        let center = ui::filmstrip::centering_offset(viewer.nav.cursor(), window_w);
        viewer.filmstrip_scroll_x = center;
        tasks.push(iced::widget::operation::scroll_to(
            ui::filmstrip::filmstrip_id(),
            iced::widget::scrollable::AbsoluteOffset { x: center, y: 0.0 },
        ));
        tasks.extend(fire_visible_thumbs(&pipeline, viewer, window_w));
    }

    Task::batch(tasks)
}

/// Fire thumbnail probes for every filmstrip cell currently in view.
fn fire_visible_thumbs(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    viewport_w: f32,
) -> Vec<Task<Message>> {
    let range =
        ui::filmstrip::visible_range(viewer.filmstrip_scroll_x, viewport_w, viewer.nav.len());
    let paths: Vec<PathBuf> = viewer.nav.files()[range].to_vec();
    paths
        .into_iter()
        .map(|p| fire_thumb(pipeline, viewer, p, ThumbUrgency::Background))
        .collect()
}

/// Put a loaded image on screen, computing zoom from its true dimensions.
fn show_loaded(
    viewer: &mut Viewer,
    path: &std::path::Path,
    image: CachedImage,
    zoom_mode: ZoomMode,
    viewport: Size,
) {
    let (w, h) = image.original_size;
    if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
    }
    viewer.pan = (0.0, 0.0);
    viewer.displayed = DisplayedImage::Full {
        allocation: image.allocation,
        original_size: image.original_size,
    };
    viewer.displayed_path = Some(path.to_path_buf());
    viewer.pending_since = None;
}

/// Put a blurred thumbnail on screen while the full image decodes. Zoom is
/// computed from the true dimensions, so geometry is identical when the
/// full image swaps in, no jump. The load stays pending (spinner included).
fn show_placeholder(
    viewer: &mut Viewer,
    path: &std::path::Path,
    thumb: Thumb,
    zoom_mode: ZoomMode,
    viewport: Size,
) {
    let (w, h) = thumb.original_size;
    if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
    }
    viewer.pan = (0.0, 0.0);
    viewer.displayed = DisplayedImage::Placeholder(thumb);
    viewer.displayed_path = Some(path.to_path_buf());
}

/// Show the cached thumbnail for `path` if there is one, otherwise clear
/// the image area. Returns true when a placeholder was shown. Either way
/// the image area now refers to `path`, never to a previous image.
fn show_placeholder_or_clear(
    viewer: &mut Viewer,
    path: &std::path::Path,
    zoom_mode: ZoomMode,
    viewport: Size,
) -> bool {
    if let Some(thumb) = viewer.thumbs.get(path).cloned() {
        show_placeholder(viewer, path, thumb, zoom_mode, viewport);
        true
    } else {
        viewer.displayed = DisplayedImage::None;
        viewer.displayed_path = None;
        false
    }
}

/// Fire a thumbnail job for `path` unless one is cached, in flight, or
/// known to fail.
fn fire_thumb(
    pipeline: &Pipeline,
    viewer: &mut Viewer,
    path: PathBuf,
    urgency: ThumbUrgency,
) -> Task<Message> {
    if viewer.thumbs.contains(&path)
        || viewer.in_flight_thumbs.contains(&path)
        || viewer.failed_thumbs.contains(&path)
    {
        return Task::none();
    }
    viewer.in_flight_thumbs.insert(path.clone());

    let load = pipeline.load_thumb(viewer.source.clone(), path.clone(), urgency);
    Task::perform(load, move |result| Message::ThumbLoaded {
        path: path.clone(),
        urgency,
        result: result.map(|data| Thumb {
            handle: Handle::from_rgba(data.width, data.height, data.pixels),
            size: (data.width, data.height),
            original_size: data.original_size,
        }),
    })
}

/// Start (or continue) background thumbnailing: up to `chains` parallel
/// job streams that work outward from the cursor until every file in the
/// directory has a thumbnail.
fn fire_thumbnailer(pipeline: &Pipeline, viewer: &mut Viewer, chains: usize) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for _ in 0..chains {
        let Some(path) = viewer.next_unthumbed() else {
            break;
        };
        tasks.push(fire_thumb(pipeline, viewer, path, ThumbUrgency::Background));
    }
    tasks
}

/// Fire a pipeline load for `path` unless it's already cached or in flight.
/// The resulting RGBA is uploaded to the GPU and lands as `MediaLoaded`.
fn fire_load(pipeline: &Pipeline, viewer: &mut Viewer, path: PathBuf, lane: Lane) -> Task<Message> {
    if viewer.cache.contains(&path) || viewer.in_flight.contains(&path) {
        return Task::none();
    }
    viewer.in_flight.insert(path.clone());

    let generation = pipeline.generation();
    let load = pipeline.load(
        viewer.source.clone(),
        path.clone(),
        DecodeOpts::default(),
        lane,
        generation,
    );

    Task::perform(load, |r| r).then(move |result| match result {
        Ok(DecodedMedia::Static(img)) => {
            let original_size = img.original_size;
            let thumb = img.thumbnail.map(|t| Thumb {
                handle: Handle::from_rgba(t.width, t.height, t.pixels),
                size: (t.width, t.height),
                original_size: t.original_size,
            });
            let handle = Handle::from_rgba(img.width, img.height, img.pixels);
            let p = path.clone();
            cache::allocate_handle(handle).map(move |upload| {
                let result = upload
                    .map(|allocation| LoadedMedia {
                        image: CachedImage {
                            allocation,
                            original_size,
                        },
                        thumb: thumb.clone(),
                    })
                    .map_err(|e| MediaError::Decode(format!("gpu upload failed: {e:?}")));
                Message::MediaLoaded {
                    path: p.clone(),
                    result,
                }
            })
        }
        Err(e) => Task::done(Message::MediaLoaded {
            path: path.clone(),
            result: Err(e),
        }),
    })
}

/// Warm the prefetch window around the cursor.
fn fire_prefetch(pipeline: &Pipeline, viewer: &mut Viewer, depth: usize) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for p in viewer.nav.peek_around(depth) {
        if viewer.is_fs() && gif::is_gif(&p) {
            tasks.push(viewer.gif_player.prefetch_decode(&p).map(Message::Gif));
        } else {
            tasks.push(fire_load(pipeline, viewer, p, Lane::Prefetch));
        }
    }
    tasks
}

/// Resolve the current image's byte size: instantly from the archive
/// index, or via an async stat for filesystem images.
fn probe_size(viewer: &mut Viewer, path: PathBuf) -> Task<Message> {
    match &viewer.source {
        Source::Fs => probe_file_size(path),
        Source::Archive(index) => {
            viewer.current_file_size = index.entry_size(&path);
            Task::none()
        }
    }
}

/// Fetch the file size off-thread, a stat on slow storage can stall for seconds and
/// must never run inside `update()`.
fn probe_file_size(path: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let size = tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            (path, size)
        },
        |(path, size)| Message::FileSizeProbed(path, size),
    )
}
