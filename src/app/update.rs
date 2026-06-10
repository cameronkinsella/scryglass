//! Update function: handles messages, mutates state, fires async tasks.

use std::collections::HashSet;
use std::path::PathBuf;

use iced::Task;
use iced::time::Instant;

use crate::cache;
use crate::config::{AppConfig, ZoomMode};
use crate::gif::{self, GifPlayer};
use crate::nav::{self, Nav};
use crate::widgets;
use crate::widgets::toolbar::OpenMenu;

use super::message::{is_context_menu_message, is_menu_message};
use super::state::{Direction, DragState, Session, Viewer};
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
                let gif_player = GifPlayer::new();
                let tasks = load_current_and_prefetch(&nav, &gif_player, app.config.prefetch_depth);
                let file_size = std::fs::metadata(nav.current())
                    .map(|m| m.len())
                    .unwrap_or(0);
                app.session = Session::Viewing(Viewer::new(nav, gif_player, file_size));
                tasks
            }
            Err(_) => Task::none(),
        },

        Message::DirectoryScanned(_start_file, Err(_err)) => Task::none(),

        Message::ImageAllocated(path, Ok(allocation)) => {
            let zoom_mode = app.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            if viewer.nav.current() == path {
                let size = allocation.size();
                if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
                    viewer.zoom = compute_zoom(zoom_mode, size.width, size.height, viewport);
                }
                viewer.pan = (0.0, 0.0);
                viewer.current_allocation = Some(allocation);
                viewer.loading = false;
            } else {
                viewer.prefetch_allocations.push(allocation);
            }
            Task::none()
        }

        Message::ImageAllocated(_path, Err(_err)) => Task::none(),

        Message::Gif(gif_msg) => {
            let zoom_mode = app.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            let is_first_frame = viewer.current_allocation.is_none()
                || (viewer.loading && matches!(&gif_msg, gif::GifMessage::FrameAllocated(..)));

            let (task, allocation) = viewer.gif_player.update(gif_msg, viewer.nav.current());

            if let Some(alloc) = allocation {
                if is_first_frame && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
                    let size = alloc.size();
                    viewer.zoom = compute_zoom(zoom_mode, size.width, size.height, viewport);
                    viewer.pan = (0.0, 0.0);
                }
                viewer.current_allocation = Some(alloc);
                viewer.loading = false;
            }

            task.map(Message::Gif)
        }

        // --- Initial press: always navigate + record hold start ---
        Message::Next => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Forward, Instant::now()));
            if viewer.loading {
                return Task::none();
            }
            navigate(app, NavTarget::Delta(Direction::Forward))
        }

        Message::Prev => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.held_direction = Some((Direction::Backward, Instant::now()));
            if viewer.loading {
                return Task::none();
            }
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
            if !past || viewer.loading {
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
            if !past || viewer.loading {
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
            app.zoom_mode = mode;
            let viewport = app.viewport_size;

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.manual_zoom = false;

            if let Some(alloc) = &viewer.current_allocation {
                let size = alloc.size();
                viewer.zoom = compute_zoom(mode, size.width, size.height, viewport);
                let img_w = size.width as f32 * viewer.zoom;
                let img_h = size.height as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        // --- Scroll-wheel zoom (toward cursor) ---
        Message::ScrollZoom(delta_y) => {
            let viewport = app.viewport_size;
            let cursor = app.last_cursor_pos;
            let toolbar_offset = if app.show_toolbar {
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

            if let Some(alloc) = &viewer.current_allocation {
                let size = alloc.size();
                let img_w = size.width as f32 * viewer.zoom;
                let img_h = size.height as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        // --- Double-click: reset zoom ---
        Message::ResetZoom => {
            let zoom_mode = app.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            viewer.manual_zoom = false;
            if let Some(alloc) = &viewer.current_allocation {
                let size = alloc.size();
                viewer.zoom = compute_zoom(zoom_mode, size.width, size.height, viewport);
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

                if let Some(alloc) = &viewer.current_allocation {
                    let size = alloc.size();
                    let img_w = size.width as f32 * viewer.zoom;
                    let img_h = size.height as f32 * viewer.zoom;
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
            let zoom_mode = app.zoom_mode;
            let viewport = app.viewport_size;

            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            if !viewer.manual_zoom
                && let Some(alloc) = &viewer.current_allocation
            {
                let s = alloc.size();
                viewer.zoom = compute_zoom(zoom_mode, s.width, s.height, viewport);
            }

            if let Some(alloc) = &viewer.current_allocation {
                let s = alloc.size();
                let img_w = s.width as f32 * viewer.zoom;
                let img_h = s.height as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        // --- Slider and filmstrip visibility ---
        Message::ToggleFilmstrip => {
            app.show_filmstrip = !app.show_filmstrip;
            recalc_viewport(app);
            Task::none()
        }

        Message::ToggleSlider => {
            app.show_slider = !app.show_slider;
            recalc_viewport(app);
            Task::none()
        }

        Message::ToggleFooter => {
            app.show_footer = !app.show_footer;
            recalc_viewport(app);
            Task::none()
        }

        Message::FilmstripScroll(delta_y) => {
            // Convert vertical scroll delta to horizontal scroll on the filmstrip.
            let offset = iced::widget::scrollable::AbsoluteOffset {
                x: -delta_y * 60.0,
                y: 0.0,
            };
            iced::widget::operation::scroll_by(widgets::filmstrip::filmstrip_id(), offset)
        }

        Message::SliderChanged(index) | Message::FilmstripClicked(index) => {
            navigate(app, NavTarget::Index(index))
        }

        // --- Toolbar visibility ---
        Message::ToggleToolbar => {
            app.show_toolbar = !app.show_toolbar;
            app.context_menu_pos = None;
            recalc_viewport(app);
            Task::none()
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

/// Move the cursor (one step or to an absolute index), then reset per-image
/// state and fire load + prefetch tasks.
fn navigate(app: &mut App, target: NavTarget) -> Task<Message> {
    let depth = app.config.prefetch_depth;
    let zoom_mode = app.zoom_mode;
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

    viewer.loading = true;
    viewer.gif_player.stop();
    viewer.prefetch_allocations.clear();
    viewer.drag = None;

    // Reset pan on navigation. Zoom is preserved only in LockZoomRatio mode.
    // Don't change zoom here. The previous image stays visible until the new
    // allocation arrives (flicker prevention), so keep the old zoom to avoid a
    // brief flash at the wrong scale. The correct zoom will be set in
    // ImageAllocated / GifMessage::FrameAllocated.
    viewer.pan = (0.0, 0.0);
    if zoom_mode != ZoomMode::LockZoomRatio {
        viewer.manual_zoom = false;
    }

    viewer.current_file_size = std::fs::metadata(viewer.nav.current())
        .map(|m| m.len())
        .unwrap_or(0);

    let keep: HashSet<PathBuf> = {
        let mut set = HashSet::new();
        set.insert(viewer.nav.current().to_path_buf());
        for p in viewer.nav.peek_around(depth) {
            set.insert(p);
        }
        set
    };
    viewer.gif_player.prune_cache(&keep);

    let current_path = viewer.nav.current().to_path_buf();
    if gif::is_gif(&current_path)
        && let Some(gif_task) = viewer.gif_player.try_start_from_cache(&current_path)
    {
        let prefetch = prefetch_neighbors(&viewer.nav, &viewer.gif_player, depth);
        return Task::batch([gif_task.map(Message::Gif), prefetch]);
    }

    load_current_and_prefetch(&viewer.nav, &viewer.gif_player, depth)
}

/// Fire allocation/decode tasks for the current image and its neighbors.
fn load_current_and_prefetch(nav: &Nav, gif_player: &GifPlayer, depth: usize) -> Task<Message> {
    let current_path = nav.current().to_path_buf();

    let current_task = if gif::is_gif(&current_path) {
        gif_player.decode_current(&current_path).map(Message::Gif)
    } else {
        let p = current_path.clone();
        cache::allocate_path(&p).map(move |result| Message::ImageAllocated(p.clone(), result))
    };

    let prefetch = prefetch_neighbors(nav, gif_player, depth);
    Task::batch([current_task, prefetch])
}

/// Fire prefetch tasks for neighbor images/GIFs.
fn prefetch_neighbors(nav: &Nav, gif_player: &GifPlayer, depth: usize) -> Task<Message> {
    let tasks: Vec<Task<Message>> = nav
        .peek_around(depth)
        .into_iter()
        .map(|p| {
            if gif::is_gif(&p) {
                gif_player.prefetch_decode(&p).map(Message::Gif)
            } else {
                let p2 = p.clone();
                cache::allocate_path(&p)
                    .map(move |result| Message::ImageAllocated(p2.clone(), result))
            }
        })
        .collect();
    Task::batch(tasks)
}
