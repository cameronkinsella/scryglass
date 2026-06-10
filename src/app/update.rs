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

use crate::cache;
use crate::config::{AppConfig, ThemeChoice, ZoomMode};
use crate::gif::{self, GifPlayer};
use crate::media::pipeline::{Lane, Pipeline};
use crate::media::registry::DecodeOpts;
use crate::media::{DecodedMedia, MediaError};
use crate::nav::{self, Nav};
use crate::ui;
use crate::ui::toolbar::OpenMenu;

use super::message::{is_context_menu_message, is_menu_message};
use super::state::{CachedImage, Direction, DisplayedImage, DragState, Session, Viewer};
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
            Ok(nav) => {
                let depth = app.config.prefetch_depth;
                let budget = app.config.cache_budget_mb * 1024 * 1024;
                let pipeline = app.pipeline.clone();

                let mut viewer = Viewer::new(nav, GifPlayer::new(), budget);
                let current = viewer.nav.current().to_path_buf();
                let mut tasks = vec![probe_file_size(current.clone())];

                if gif::is_gif(&current) {
                    tasks.push(viewer.gif_player.decode_current(&current).map(Message::Gif));
                } else {
                    tasks.push(fire_load(&pipeline, &mut viewer, current, Lane::Current));
                }
                tasks.extend(fire_prefetch(&pipeline, &mut viewer, depth));

                app.session = Session::Viewing(Box::new(viewer));
                Task::batch(tasks)
            }
            Err(_) => Task::none(),
        },

        Message::DirectoryScanned(_start_file, Err(_err)) => Task::none(),

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
                Ok(image) => {
                    // A "stale" decode is a free prefetch, so cache it anyway.
                    viewer
                        .cache
                        .insert(path.clone(), image.clone(), image.byte_cost());
                    if viewer.nav.current() == path {
                        show_loaded(viewer, image, zoom_mode, viewport);
                    }
                    let pinned = viewer.pinned_paths(depth);
                    viewer.cache.evict_over_budget(&pinned);
                    Task::none()
                }
                Err(MediaError::Cancelled) => {
                    // Cancelled loads that are still wanted get re-fired with
                    // the live generation.
                    if viewer.nav.current() == path {
                        fire_load(&pipeline, viewer, path, Lane::Current)
                    } else if viewer.pinned_paths(depth).contains(&path) {
                        fire_load(&pipeline, viewer, path, Lane::Prefetch)
                    } else {
                        Task::none()
                    }
                }
                Err(_err) => {
                    if viewer.nav.current() == path {
                        // Stop the spinner, the previous image stays visible.
                        viewer.pending_since = None;
                    }
                    Task::none()
                }
            }
        }

        Message::FileSizeProbed(path, size) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.current_file_size = Some(size);
            }
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
                viewer.pending_since = None;
            }

            task.map(Message::Gif)
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
            let path = viewer.nav.current().to_path_buf();
            Task::perform(
                async move { crate::platform::copy_image_to_clipboard(&path) },
                |_| Message::DismissOverlay, // no-op follow-up
            )
        }

        Message::CopyFilePath => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path_str = viewer.nav.current().to_string_lossy().to_string();
            iced::clipboard::write(path_str)
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
            iced::clipboard::write(name)
        }

        Message::OpenImageLocation => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::reveal_in_file_manager(viewer.nav.current());
            Task::none()
        }

        Message::ImageProperties => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            crate::platform::show_properties(viewer.nav.current());
            Task::none()
        }
    }
}

/// Persist the current config in the background. Saving is fire-and-forget:
/// the viewer must never wait on it.
fn save_config(app: &App) -> Task<Message> {
    Task::future(app.config.clone().save()).discard()
}

/// Shared logic for opening a path (from drop, dialog, or CLI argument).
///
/// A file opens at that file within its parent directory. A directory
/// opens at its first supported image.
pub fn open_path(path: PathBuf) -> Task<Message> {
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
    let depth = app.config.prefetch_depth;
    let zoom_mode = app.config.zoom_mode;
    let viewport = app.viewport_size;
    let pipeline = app.pipeline.clone();
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };

    match target {
        NavTarget::Delta(Direction::Forward) => viewer.nav.next(),
        NavTarget::Delta(Direction::Backward) => viewer.nav.prev(),
        NavTarget::Index(index) => {
            // Don't navigate if already at this index.
            if viewer.nav.cursor() == index {
                return Task::none();
            }
            viewer.nav.set_cursor(index);
        }
    }

    // Everything in flight for the old position is now stale.
    pipeline.bump_generation();

    viewer.gif_player.stop();
    viewer.drag = None;

    // Reset pan on navigation. Zoom is preserved only in LockZoomRatio mode.
    // The previous image stays visible until the new one is ready (flicker
    // prevention). Its zoom is kept until then to avoid a flash at the
    // wrong scale.
    viewer.pan = (0.0, 0.0);
    if zoom_mode != ZoomMode::LockZoomRatio {
        viewer.manual_zoom = false;
    }

    viewer.current_file_size = None;

    // The GIF decode cache prunes by window, the image cache by byte budget.
    let keep = viewer.pinned_paths(depth);
    viewer.gif_player.prune_cache(&keep);

    let current = viewer.nav.current().to_path_buf();
    let mut tasks = vec![probe_file_size(current.clone())];

    if gif::is_gif(&current) {
        viewer.pending_since = Some(Instant::now());
        if let Some(gif_task) = viewer.gif_player.try_start_from_cache(&current) {
            tasks.push(gif_task.map(Message::Gif));
        } else {
            tasks.push(viewer.gif_player.decode_current(&current).map(Message::Gif));
        }
    } else if let Some(cached) = viewer.cache.get(&current).cloned() {
        // Instant display, the common case within the prefetch window.
        show_loaded(viewer, cached, zoom_mode, viewport);
    } else {
        viewer.pending_since = Some(Instant::now());
        tasks.push(fire_load(&pipeline, viewer, current, Lane::Current));
    }

    tasks.extend(fire_prefetch(&pipeline, viewer, depth));

    let pinned = viewer.pinned_paths(depth);
    viewer.cache.evict_over_budget(&pinned);

    Task::batch(tasks)
}

/// Put a loaded image on screen, computing zoom from its true dimensions.
fn show_loaded(viewer: &mut Viewer, image: CachedImage, zoom_mode: ZoomMode, viewport: Size) {
    let (w, h) = image.original_size;
    if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
        viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
    }
    viewer.pan = (0.0, 0.0);
    viewer.displayed = DisplayedImage::Full {
        allocation: image.allocation,
        original_size: image.original_size,
    };
    viewer.pending_since = None;
}

/// Fire a pipeline load for `path` unless it's already cached or in flight.
/// The resulting RGBA is uploaded to the GPU and lands as `MediaLoaded`.
fn fire_load(pipeline: &Pipeline, viewer: &mut Viewer, path: PathBuf, lane: Lane) -> Task<Message> {
    if viewer.cache.contains(&path) || viewer.in_flight.contains(&path) {
        return Task::none();
    }
    viewer.in_flight.insert(path.clone());

    let generation = pipeline.generation();
    let load = pipeline.load(path.clone(), DecodeOpts::default(), lane, generation);

    Task::perform(load, |r| r).then(move |result| match result {
        Ok(DecodedMedia::Static(img)) => {
            let original_size = img.original_size;
            let handle = Handle::from_rgba(img.width, img.height, img.pixels);
            let p = path.clone();
            cache::allocate_handle(handle).map(move |upload| {
                let result = upload
                    .map(|allocation| CachedImage {
                        allocation,
                        original_size,
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
        if gif::is_gif(&p) {
            tasks.push(viewer.gif_player.prefetch_decode(&p).map(Message::Gif));
        } else {
            tasks.push(fire_load(pipeline, viewer, p, Lane::Prefetch));
        }
    }
    tasks
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
