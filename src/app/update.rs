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

use crate::anim::{AnimMessage, AnimPlayer};
use crate::cache;
use crate::config::{AppConfig, ThemeChoice, ZoomMode};
use crate::media::archive::{self, ArchiveIndex};
use crate::media::pipeline::{Lane, Pipeline, Source, ThumbUrgency};
use crate::media::registry::DecodeOpts;
use crate::media::{DecodedMedia, MediaError};
use crate::nav::{self, Nav};
use crate::ui;
use crate::ui::toast::{Toast, ToastKind};
use crate::ui::toolbar::OpenMenu;

use super::Modal;
use super::message::{is_context_menu_message, is_menu_message, is_viewer_interaction};
use super::state::{
    CachedImage, Direction, DisplayedImage, DragState, LoadedMedia, Session, SliderDrag, Thumb,
    Viewer,
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

    // A modal dialog owns the keyboard: viewer interactions go inert so
    // text typed into an input never navigates or deletes.
    if app.modal.is_some() && is_viewer_interaction(&message) {
        return Task::none();
    }

    match message {
        Message::FileDropped(path) => {
            app.opening_since = Some(Instant::now());
            open_path(path)
        }

        Message::DirectoryScanned(start_file, opened_dir, Ok(files)) => {
            app.opening_since = None;
            match Nav::new(files, &start_file) {
                Ok(nav) => open_viewer(app, nav, Source::Fs, opened_dir),
                Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't open: {e}")),
            }
        }

        Message::DirectoryScanned(_start_file, _opened_dir, Err(err)) => {
            app.opening_since = None;
            push_toast(app, ToastKind::Error, format!("Couldn't open: {err}"))
        }

        Message::ArchiveScanned(archive_path, Ok(index)) => {
            app.opening_since = None;
            let entries = index.image_entries();
            let start = entries.first().cloned();
            match start.and_then(|s| Nav::new(entries, &s).ok()) {
                Some(nav) => open_viewer(app, nav, Source::Archive(index), true),
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

        Message::ArchiveScanned(_archive_path, Err(err)) => {
            app.opening_since = None;
            push_toast(
                app,
                ToastKind::Error,
                format!("Couldn't open archive: {err}"),
            )
        }

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
                Ok(LoadedMedia::Static { image, thumb }) => {
                    // A "stale" decode is a free prefetch, so cache it anyway.
                    viewer
                        .cache
                        .insert(path.clone(), image.clone(), image.byte_cost());
                    if let Some(thumb) = thumb {
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
                Ok(LoadedMedia::Animated { anim, thumb }) => {
                    if let Some(thumb) = thumb {
                        let cost = thumb.byte_cost();
                        viewer.thumbs.insert(path.clone(), thumb, cost);
                    }
                    viewer.anim_player.insert(path.clone(), anim);
                    // Start playback if this is the image on screen.
                    let play = if viewer.nav.current() == path {
                        viewer
                            .anim_player
                            .try_start_from_cache(&path)
                            .map(|t| t.map(Message::Anim))
                            .unwrap_or_else(Task::none)
                    } else {
                        Task::none()
                    };
                    Task::batch([play, resolve_pending_nav(app)])
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

        Message::Anim(anim_msg) => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            let is_first_frame = matches!(viewer.displayed, DisplayedImage::None)
                || (viewer.pending_since.is_some()
                    && matches!(&anim_msg, AnimMessage::FrameAllocated(..)));

            let (task, allocation) = viewer.anim_player.update(anim_msg, viewer.nav.current());

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
            Task::batch([task.map(Message::Anim), resolve_pending_nav(app)])
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

        Message::First => navigate(app, NavTarget::Index(0)),

        Message::Last => {
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let last = viewer.nav.len().saturating_sub(1);
            navigate(app, NavTarget::Index(last))
        }

        // --- Fullscreen ---
        Message::ToggleFullscreen => {
            app.fullscreen = !app.fullscreen;
            recalc_viewport(app);
            let mode = if app.fullscreen {
                iced::window::Mode::Fullscreen
            } else {
                iced::window::Mode::Windowed
            };
            iced::window::latest().and_then(move |id| iced::window::set_mode(id, mode))
        }

        Message::Escape => {
            if app.modal.is_some() {
                app.modal = None;
                return Task::none();
            }
            if app.help_open {
                app.help_open = false;
                return Task::none();
            }
            if app.fullscreen {
                return update(app, Message::ToggleFullscreen);
            }
            app.open_menu = None;
            app.context_menu_pos = None;
            Task::none()
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

        Message::ToggleSortMenu => {
            app.open_menu = if app.open_menu == Some(OpenMenu::Sort) {
                None
            } else {
                Some(OpenMenu::Sort)
            };
            Task::none()
        }

        // --- Sorting ---
        Message::SetSortKey(key) => {
            app.open_menu = None;
            app.config.sort_key = key;
            Task::batch([save_config(app), fire_resort(app)])
        }

        Message::ToggleSortDirection => {
            app.config.sort_desc = !app.config.sort_desc;
            Task::batch([save_config(app), fire_resort(app)])
        }

        Message::Resorted(files) => {
            let window_w = app.window_size.width;
            let show_filmstrip = app.config.show_filmstrip;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.nav.replace_files(files);

            // A folder or archive open should land on the first image of
            // the configured sort, not wherever the pre-sort start file
            // ended up.
            if viewer.resort_to_first {
                viewer.resort_to_first = false;
                if viewer.nav.cursor() != 0 {
                    return complete_navigation(app, 0, true);
                }
            }

            let mut tasks = Vec::new();
            if show_filmstrip {
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

        Message::FileDialogResult(Some(path)) => {
            app.opening_since = Some(Instant::now());
            open_path(path)
        }
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

        // --- Keyboard zoom: steps about the viewport center ---
        Message::ZoomStep(direction) => {
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };

            let old_zoom = viewer.zoom;
            let factor = if direction > 0 {
                ZOOM_STEP
            } else {
                1.0 / ZOOM_STEP
            };
            viewer.zoom = (old_zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            viewer.manual_zoom = true;
            viewer.pan = pan_for_zoom_toward_cursor(viewer.pan, viewer.zoom / old_zoom, (0.0, 0.0));

            if let Some((w, h)) = viewer.displayed.original_size() {
                let img_w = w as f32 * viewer.zoom;
                let img_h = h as f32 * viewer.zoom;
                viewer.pan = clamp_pan(viewer.pan, img_w, img_h, viewport);
            }
            Task::none()
        }

        Message::ZoomActual => {
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.zoom = 1.0;
            viewer.manual_zoom = true;
            if let Some((w, h)) = viewer.displayed.original_size() {
                viewer.pan = clamp_pan(viewer.pan, w as f32, h as f32, viewport);
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

            // Any mouse movement keeps the video controls up.
            if viewer.video.is_some() {
                viewer.video_controls_until = Some(Instant::now() + super::VIDEO_CONTROLS_TIMEOUT);
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
            Task::none()
        }

        Message::CursorLeft => {
            // Mouse left the window: hide the controls right away.
            if let Some(viewer) = app.viewer_mut() {
                viewer.video_controls_until = None;
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
            // Remember the size for the next session (persisted on close).
            app.config.window_width = size.width;
            app.config.window_height = size.height;
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

        // --- Slider scrub: thumb follows the hand and display live-follows
        // through loaded files, the sticky bubble covers cold stretches ---
        Message::SliderChanged(index) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let index = index % viewer.nav.len().max(1);
            let path = viewer.nav.files()[index].to_path_buf();
            // GIFs only count as scrubbable by their thumb. Playback
            // starts on release, not mid-drag.
            let scrubbable = viewer.cache.contains(&path) || viewer.thumbs.contains(&path);
            let bubble = viewer.slider_drag.map(|d| d.bubble).unwrap_or(false) || !scrubbable;
            viewer.slider_drag = Some(SliderDrag {
                target: index,
                bubble,
            });

            if scrubbable && index != viewer.nav.cursor() {
                scrub_to(app, index)
            } else {
                Task::none()
            }
        }

        Message::SliderReleased => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let Some(drag) = viewer.slider_drag.take() else {
                return Task::none();
            };
            if drag.target == viewer.nav.cursor() {
                // Display already followed here mid-drag, so run the deferred
                // heavy tail (probe, prefetch, filmstrip centering).
                complete_navigation(app, drag.target, true)
            } else {
                navigate(app, NavTarget::Index(drag.target))
            }
        }

        Message::FilmstripClicked(index) => navigate(app, NavTarget::Index(index)),

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

        // --- View rotation (non-destructive, resets on navigation) ---
        Message::Rotate(turns) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            // Rotation operates on the displayed full texture. Placeholders
            // sharpen first, then rotate.
            if !matches!(viewer.displayed, DisplayedImage::Full { .. }) {
                return Task::none();
            }
            viewer.rotation = (viewer.rotation + turns) % 4;
            fire_rotate(viewer)
        }

        Message::ViewRotated { path, baked, image } => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            // Ignore results for an image we've navigated away from
            // (rotation resets on navigation anyway).
            if viewer.nav.current() != path
                || !matches!(viewer.displayed, DisplayedImage::Full { .. })
            {
                return Task::none();
            }

            let (w, h) = image.original_size;
            viewer.displayed = DisplayedImage::Full {
                allocation: image.allocation,
                original_size: image.original_size,
            };
            viewer.displayed_rotation = baked;
            viewer.pan = (0.0, 0.0);
            if !viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
            }

            // Catch up if more rotations were requested mid-flight.
            fire_rotate(viewer)
        }

        Message::ToggleCheckerboard => {
            app.config.show_checkerboard = !app.config.show_checkerboard;
            save_config(app)
        }

        Message::ToggleHelp => {
            app.help_open = !app.help_open;
            Task::none()
        }

        // --- File operations (filesystem sources only) ---
        Message::RequestDelete => {
            let target = match file_op_target(app) {
                Ok(target) => target,
                Err(refusal) => return refusal,
            };
            if app.config.confirm_delete {
                app.modal = Some(Modal::ConfirmDelete(target));
                Task::none()
            } else {
                fire_delete(app, target)
            }
        }

        Message::ConfirmDeleteNow => {
            let Some(Modal::ConfirmDelete(path)) = app.modal.take() else {
                return Task::none();
            };
            fire_delete(app, path)
        }

        Message::DeleteFinished(path, result) => match result {
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't delete: {e}")),
            Ok(()) => {
                let purge = purge_disk_thumb(&app.pipeline, &path);
                let Some(viewer) = app.viewer_mut() else {
                    return purge;
                };
                viewer.cache.remove(&path);
                viewer.thumbs.remove(&path);
                viewer.anim_player.remove(&path);
                viewer.failed_thumbs.remove(&path);

                if !viewer.nav.remove(&path) {
                    app.session = Session::Empty;
                    let toast = push_toast(app, ToastKind::Info, "Moved to Recycle Bin".into());
                    return Task::batch([purge, toast]);
                }
                let cursor = viewer.nav.cursor();
                let nav = complete_navigation(app, cursor, true);
                let toast = push_toast(app, ToastKind::Info, "Moved to Recycle Bin".into());
                Task::batch([purge, nav, toast])
            }
        },

        Message::RequestRename => {
            let target = match file_op_target(app) {
                Ok(target) => target,
                Err(refusal) => return refusal,
            };
            let input = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            app.modal = Some(Modal::Rename { input });
            Task::batch([
                iced::widget::operation::focus(ui::dialogs::rename_input_id()),
                iced::widget::operation::select_all(ui::dialogs::rename_input_id()),
            ])
        }

        Message::RenameInput(text) => {
            if let Some(Modal::Rename { input }) = &mut app.modal {
                *input = text;
            }
            Task::none()
        }

        Message::CommitRename => {
            let Some(Modal::Rename { input }) = &app.modal else {
                return Task::none();
            };
            let name = match validate_rename(input) {
                Ok(name) => name,
                // Invalid input: explain and keep the dialog open.
                Err(e) => return push_toast(app, ToastKind::Error, e),
            };
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let old = viewer.nav.current().to_path_buf();
            let new = old.parent().unwrap_or(std::path::Path::new("")).join(name);
            app.modal = None;
            if new == old {
                return Task::none();
            }

            Task::perform(
                async move {
                    if tokio::fs::try_exists(&new).await.unwrap_or(false) {
                        let err = "a file with that name already exists".to_string();
                        return (old, new, Err(err));
                    }
                    let result = tokio::fs::rename(&old, &new)
                        .await
                        .map_err(|e| e.to_string());
                    (old, new, result)
                },
                |(old, new, result)| Message::RenameFinished(old, new, result),
            )
        }

        Message::RenameFinished(old, new, result) => match result {
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't rename: {e}")),
            Ok(()) => {
                let purge = purge_disk_thumb(&app.pipeline, &old);
                let Some(viewer) = app.viewer_mut() else {
                    return purge;
                };
                viewer.nav.rename(&old, new.clone());
                // Carry cached textures over to the new key.
                if let Some(image) = viewer.cache.remove(&old) {
                    let cost = image.byte_cost();
                    viewer.cache.insert(new.clone(), image, cost);
                }
                if let Some(thumb) = viewer.thumbs.remove(&old) {
                    let cost = thumb.byte_cost();
                    viewer.thumbs.insert(new.clone(), thumb, cost);
                }
                viewer.anim_player.remove(&old);
                if viewer.displayed_path.as_deref() == Some(&*old) {
                    viewer.displayed_path = Some(new.clone());
                }
                if let Some((p, _)) = &mut viewer.exif
                    && *p == old
                {
                    *p = new;
                }
                purge
            }
        },

        Message::ModalSubmit => match &app.modal {
            Some(Modal::ConfirmDelete(_)) => update(app, Message::ConfirmDeleteNow),
            Some(Modal::Rename { .. }) => update(app, Message::CommitRename),
            Some(Modal::Settings) => update(app, Message::ModalCancel),
            None => Task::none(),
        },

        Message::ModalCancel => {
            app.modal = None;
            Task::none()
        }

        // --- Settings ---
        Message::OpenSettings => {
            app.open_menu = None;
            app.modal = Some(Modal::Settings);
            app.disk_cache_size = None;
            app.associations_registered = crate::platform::file_associations_registered();
            probe_disk_cache_size(&app.pipeline)
        }

        Message::ToggleFileAssociations => {
            let result = if app.associations_registered {
                crate::platform::unregister_file_associations().map(|_| false)
            } else {
                crate::platform::register_file_associations().map(|_| true)
            };
            match result {
                Ok(registered) => {
                    app.associations_registered = registered;
                    let note = if registered {
                        "Done. To make scryglass the default, pick it under \
                         Windows Settings > Default apps."
                    } else {
                        "scryglass no longer appears in the Open with menu."
                    };
                    push_toast(app, ToastKind::Info, note.into())
                }
                Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't update: {e}")),
            }
        }

        Message::DiskCacheSize(bytes) => {
            app.disk_cache_size = Some(bytes);
            Task::none()
        }

        Message::ClearDiskThumbs => {
            let Some(disk) = app.pipeline.disk() else {
                return Task::none();
            };
            app.disk_cache_size = None;
            let pipeline = app.pipeline.clone();
            Task::batch([
                Task::future(async move {
                    let _ = tokio::task::spawn_blocking(move || disk.clear()).await;
                })
                .then(move |_| probe_disk_cache_size(&pipeline)),
                push_toast(app, ToastKind::Info, "Thumbnail store cleared".into()),
            ])
        }

        Message::SetPrefetchDepth(depth) => {
            app.config.prefetch_depth = depth.clamp(1, 10);
            save_config(app)
        }

        Message::SetCacheBudget(megabytes) => {
            app.config.cache_budget_mb = megabytes.clamp(128, 4096);
            let budget = app.config.cache_budget_mb * 1024 * 1024;
            let depth = app.config.prefetch_depth;
            if let Some(viewer) = app.viewer_mut() {
                viewer.cache.set_budget(budget);
                let pinned = viewer.pinned_paths(depth);
                viewer.cache.evict_over_budget(&pinned);
            }
            save_config(app)
        }

        Message::ToggleReadOnly => {
            app.config.read_only = !app.config.read_only;
            save_config(app)
        }

        Message::ToggleConfirmDelete => {
            app.config.confirm_delete = !app.config.confirm_delete;
            save_config(app)
        }

        Message::ToggleDiskThumbs => {
            app.config.disk_thumbs = !app.config.disk_thumbs;
            // Swap the store in the running pipeline. In-flight loads
            // keep the snapshot they captured, new ones see the change.
            app.pipeline
                .set_disk(crate::media::disk_thumbs::DiskThumbs::create(
                    app.config.disk_thumbs,
                ));
            app.disk_cache_size = None;
            Task::batch([save_config(app), probe_disk_cache_size(&app.pipeline)])
        }

        Message::CopyImageFinished(result) => match result {
            Ok(()) => push_toast(app, ToastKind::Info, "Image copied".into()),
            Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't copy: {e}")),
        },

        // --- Video playback ---
        Message::VideoTick => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let Some(session) = viewer.video.as_mut() else {
                return Task::none();
            };

            let Some(frame) = session.poll() else {
                // Only a session with nothing left to show is finished, since
                // queued frames still drain through poll() above.
                if session.finished() {
                    if session.looping {
                        viewer.video = Some(session.reopen_at(std::time::Duration::ZERO));
                    } else if session.playing {
                        session.pause();
                    }
                }
                return Task::none();
            };
            let path = viewer.nav.current().to_path_buf();
            let (width, height) = (frame.width, frame.height);
            let handle = Handle::from_rgba(width, height, frame.rgba);
            cache::allocate_handle(handle).map(move |upload| match upload {
                Ok(allocation) => Message::VideoFrame {
                    path: path.clone(),
                    image: CachedImage {
                        allocation,
                        original_size: (width, height),
                    },
                },
                Err(_) => Message::SpinnerTick,
            })
        }

        Message::VideoExtracted { entry, result } => {
            let video_volume = app.config.video_volume;
            let video_muted = app.config.video_muted;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            if viewer.video_extracting.as_deref() == Some(&*entry) {
                viewer.video_extracting = None;
            }
            // Navigated away while extracting: discard the temp file.
            if viewer.nav.current() != entry {
                if let Ok(temp) = result {
                    drop(crate::video::TempFileGuard::new(temp));
                }
                return Task::none();
            }
            match result {
                Err(e) => {
                    viewer.pending_since = None;
                    push_toast(app, ToastKind::Error, format!("Couldn't play video: {e}"))
                }
                Ok(temp) => {
                    let mut session = crate::video::VideoSession::open(
                        temp.clone(),
                        std::time::Duration::ZERO,
                        video_volume,
                        video_muted,
                        false,
                    );
                    session.temp = Some(crate::video::TempFileGuard::new(temp));
                    viewer.video = Some(session);
                    Task::none()
                }
            }
        }

        Message::VideoFrame { path, image } => {
            let zoom_mode = app.config.zoom_mode;
            let viewport = app.viewport_size;
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            if viewer.nav.current() != path || viewer.video.is_none() {
                return Task::none();
            }
            let first =
                matches!(viewer.displayed, DisplayedImage::None) || viewer.pending_since.is_some();
            let (w, h) = image.original_size;
            if first && (!viewer.manual_zoom || zoom_mode != ZoomMode::LockZoomRatio) {
                viewer.zoom = compute_zoom(zoom_mode, w, h, viewport);
                viewer.pan = (0.0, 0.0);
            }
            viewer.displayed = DisplayedImage::Full {
                allocation: image.allocation,
                original_size: image.original_size,
            };
            viewer.displayed_path = Some(path);
            viewer.pending_since = None;
            Task::none()
        }

        Message::VideoPlayPause => {
            if let Some(viewer) = app.viewer_mut()
                && let Some(session) = viewer.video.as_mut()
            {
                if session.playing {
                    session.pause();
                } else {
                    session.play();
                }
            }
            Task::none()
        }

        Message::VideoSeekDrag(secs) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.video.is_some()
            {
                viewer.video_seek_drag = Some(secs);
            }
            Task::none()
        }

        Message::VideoSeekRelease => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let (Some(target), Some(session)) =
                (viewer.video_seek_drag.take(), viewer.video.as_ref())
            else {
                return Task::none();
            };
            viewer.video =
                Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
            Task::none()
        }

        Message::VideoSeekBy(delta) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let Some(session) = viewer.video.as_ref() else {
                return Task::none();
            };
            let mut target = session.position().as_secs_f64() + delta;
            if let Some(duration) = session.duration() {
                target = target.min(duration.as_secs_f64() - 0.5);
            }
            viewer.video =
                Some(session.reopen_at(std::time::Duration::from_secs_f64(target.max(0.0))));
            Task::none()
        }

        Message::VideoSetVolume(volume) => {
            app.config.video_volume = volume.clamp(0.0, 1.0);
            app.config.video_muted = false;
            if let Some(viewer) = app.viewer_mut()
                && let Some(session) = viewer.video.as_mut()
            {
                session.set_volume(volume);
            }
            save_config(app)
        }

        Message::VideoNudgeVolume(delta) => {
            let volume = (app.config.video_volume + delta).clamp(0.0, 1.0);
            update(app, Message::VideoSetVolume(volume))
        }

        Message::VideoToggleMute => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let Some(session) = viewer.video.as_mut() else {
                return Task::none();
            };
            session.toggle_mute();
            app.config.video_muted = app
                .viewer()
                .and_then(|v| v.video.as_ref())
                .is_some_and(|s| s.muted);
            save_config(app)
        }

        Message::VideoToggleLoop => {
            if let Some(viewer) = app.viewer_mut()
                && let Some(session) = viewer.video.as_mut()
            {
                session.looping = !session.looping;
            }
            Task::none()
        }

        // --- Window close: persist config (window size included) first ---
        Message::CloseRequested(id) => {
            let config = app.config.clone();
            Task::future(config.save()).then(move |_| iced::window::close(id))
        }

        // --- Info panel ---
        Message::ToggleInfo => {
            app.config.show_info = !app.config.show_info;
            recalc_viewport(app);
            let probe = if app.config.show_info {
                fire_exif(app)
            } else {
                Task::none()
            };
            Task::batch([save_config(app), probe])
        }

        Message::ExifLoaded(path, fields) => {
            if let Some(viewer) = app.viewer_mut()
                && viewer.nav.current() == path
            {
                viewer.exif = Some((path, fields));
            }
            Task::none()
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
            // Copy the displayed pixels as a real bitmap. It works for any
            // source (archives included) and pastes into image editors.
            let DisplayedImage::Full { allocation, .. } = &viewer.displayed else {
                return push_toast(app, ToastKind::Info, "Image is still loading".into());
            };
            let handle = allocation.handle().clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || copy_bitmap(&handle))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                Message::CopyImageFinished,
            )
        }

        Message::CopyFile => {
            app.context_menu_pos = None;
            let Some(viewer) = app.viewer() else {
                return Task::none();
            };
            let path = viewer.current_disk_path();
            let copy = Task::future(async move {
                crate::platform::copy_file_to_clipboard(&path);
            })
            .discard();
            Task::batch([
                copy,
                push_toast(app, ToastKind::Info, "File copied".to_string()),
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

/// Rotate the displayed texture to match the desired view rotation.
/// Rotation happens on the pixels (off-thread) so every bit of zoom, pan,
/// and crop math keeps working on the rotated dimensions unchanged. The
/// cache keeps the unrotated original.
fn fire_rotate(viewer: &mut Viewer) -> Task<Message> {
    if viewer.rotation == viewer.displayed_rotation {
        return Task::none();
    }
    let DisplayedImage::Full { allocation, .. } = &viewer.displayed else {
        return Task::none();
    };
    let delta = (4 + viewer.rotation - viewer.displayed_rotation) % 4;
    let baked = viewer.rotation;
    let handle = allocation.handle().clone();
    let path = viewer.nav.current().to_path_buf();

    Task::perform(
        async move { tokio::task::spawn_blocking(move || rotate_pixels(&handle, delta)).await },
        |r| r.ok().flatten(),
    )
    .then(move |rotated| {
        let Some((width, height, pixels)) = rotated else {
            return Task::none();
        };
        let p = path.clone();
        cache::allocate_handle(Handle::from_rgba(width, height, pixels)).map(move |upload| {
            match upload {
                Ok(allocation) => Message::ViewRotated {
                    path: p.clone(),
                    baked,
                    image: CachedImage {
                        allocation,
                        original_size: (width, height),
                    },
                },
                // Upload failures leave the previous texture in place.
                Err(_) => Message::SpinnerTick,
            }
        })
    })
}

/// Rotate RGBA pixels behind a handle by quarter turns clockwise.
fn rotate_pixels(handle: &Handle, turns: u8) -> Option<(u32, u32, Vec<u8>)> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return None;
    };
    let buffer = image::RgbaImage::from_raw(*width, *height, pixels.to_vec())?;
    let img = image::DynamicImage::ImageRgba8(buffer);
    let rotated = match turns % 4 {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    };
    let out = rotated.into_rgba8();
    let (w, h) = out.dimensions();
    Some((w, h, out.into_raw()))
}

/// The current file, if file operations are allowed on it: requires a
/// filesystem source and read-only mode off. Refusals return the toast
/// task explaining why.
fn file_op_target(app: &mut App) -> Result<PathBuf, Task<Message>> {
    let Some(viewer) = app.viewer() else {
        return Err(Task::none());
    };
    if !viewer.is_fs() {
        return Err(push_toast(
            app,
            ToastKind::Info,
            "Archive entries can't be modified".into(),
        ));
    }
    if app.config.read_only {
        return Err(push_toast(
            app,
            ToastKind::Info,
            "Read-only mode is on".into(),
        ));
    }
    Ok(app
        .viewer()
        .map(|v| v.nav.current().to_path_buf())
        .unwrap_or_default())
}

/// Move a file to the recycle bin, off-thread.
fn fire_delete(app: &mut App, path: PathBuf) -> Task<Message> {
    app.modal = None;
    Task::perform(
        async move {
            let p = path.clone();
            let result = tokio::task::spawn_blocking(move || trash::delete(&p))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            (path, result)
        },
        |(path, result)| Message::DeleteFinished(path, result),
    )
}

/// Put the displayed image on the clipboard as bitmap data.
fn copy_bitmap(handle: &Handle) -> Result<(), String> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return Err("no pixel data available".into());
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: *width as usize,
            height: *height as usize,
            bytes: std::borrow::Cow::Borrowed(pixels),
        })
        .map_err(|e| e.to_string())
}

/// Measure the disk thumbnail store, off-thread.
fn probe_disk_cache_size(pipeline: &Pipeline) -> Task<Message> {
    let Some(disk) = pipeline.disk() else {
        return Task::done(Message::DiskCacheSize(0));
    };
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || disk.total_size())
                .await
                .unwrap_or(0)
        },
        Message::DiskCacheSize,
    )
}

/// Drop a deleted/renamed file's entry from the persistent thumbnail
/// store so the thumbnail can't outlive the file.
fn purge_disk_thumb(pipeline: &Pipeline, path: &std::path::Path) -> Task<Message> {
    let Some(disk) = pipeline.disk() else {
        return Task::none();
    };
    let container = path
        .parent()
        .unwrap_or(std::path::Path::new(""))
        .to_path_buf();
    let name = path.file_name().unwrap_or_default().to_owned();
    Task::future(async move {
        let _ = tokio::task::spawn_blocking(move || disk.remove(&container, &name)).await;
    })
    .discard()
}

/// Validate a rename input: non-empty, no path/invalid characters, and a
/// supported image extension (anything else would vanish from the list).
fn validate_rename(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("Name can't be empty".into());
    }
    if name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']) {
        return Err("Name contains invalid characters".into());
    }
    let supported = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(crate::config::AppConfig::is_supported_extension);
    if !supported {
        return Err("Name must keep a supported image extension".into());
    }
    Ok(name.to_string())
}

/// Fetch EXIF fields for the current image (info panel).
fn fire_exif(app: &mut App) -> Task<Message> {
    let Some(viewer) = app.viewer_mut() else {
        return Task::none();
    };
    let path = viewer.nav.current().to_path_buf();
    // Reuse data already loaded for this file, clear it otherwise.
    if viewer.exif.as_ref().is_some_and(|(p, _)| *p == path) {
        return Task::none();
    }
    viewer.exif = None;
    let load = crate::media::pipeline::load_info(viewer.source.clone(), path.clone());
    Task::perform(load, move |fields| {
        Message::ExifLoaded(path.clone(), fields)
    })
}

/// Re-sort the open folder by the configured key off-thread. Metadata
/// (date/size) is fetched only when the key needs it, archives use their
/// index and never touch the filesystem.
fn fire_resort(app: &App) -> Task<Message> {
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
        Message::Resorted,
    )
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
fn open_viewer(app: &mut App, nav: Nav, source: Source, opened_container: bool) -> Task<Message> {
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
        |(path, opened_dir, result)| Message::DirectoryScanned(path, opened_dir, result),
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

    let tasks = vec![
        fire_thumb(&pipeline, viewer, target_path.clone(), ThumbUrgency::Urgent),
        fire_load(&pipeline, viewer, target_path, Lane::Current),
    ];
    Task::batch(tasks)
}

/// Mid-drag scrub step onto an already-loaded file: move display, title,
/// and cursor together with minimal side effects: no generation bump, no
/// prefetch, no probes, no filmstrip centering. Those run once on release.
fn scrub_to(app: &mut App, index: usize) -> Task<Message> {
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

/// Begin video playback for the current file: open a session directly
/// for filesystem files, or extract the archive entry to a temp file
/// first (FFmpeg needs a real file, the spinner covers the wait).
fn start_video(viewer: &mut Viewer, current: PathBuf, volume: f32, muted: bool) -> Task<Message> {
    // Show the controls briefly on open, like most players.
    viewer.video_controls_until = Some(Instant::now() + super::VIDEO_CONTROLS_TIMEOUT);
    match &viewer.source {
        Source::Fs => {
            viewer.video = Some(crate::video::VideoSession::open(
                current,
                std::time::Duration::ZERO,
                volume,
                muted,
                false,
            ));
            Task::none()
        }
        Source::Archive(index) => {
            if viewer.video_extracting.as_deref() == Some(&*current) {
                return Task::none();
            }
            viewer.video_extracting = Some(current.clone());
            fire_video_extract(index.clone(), current)
        }
    }
}

/// Extract an archive video entry to a uniquely-named temp file,
/// off-thread. The whole entry is written out before playback starts.
fn fire_video_extract(index: Arc<ArchiveIndex>, entry: PathBuf) -> Task<Message> {
    Task::perform(
        async move {
            let e = entry.clone();
            let result = tokio::task::spawn_blocking(move || {
                static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let bytes = index.read(&e).map_err(|err| err.to_string())?;
                let dir = crate::video::extraction_dir();
                std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
                let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let name = e.file_name().unwrap_or_default().to_string_lossy();
                let file = dir.join(format!("{}-{unique}-{name}", std::process::id()));
                std::fs::write(&file, bytes).map_err(|err| err.to_string())?;
                Ok(file)
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|r| r);
            (entry, result)
        },
        |(entry, result)| Message::VideoExtracted { entry, result },
    )
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
    let video_volume = app.config.video_volume;
    let video_muted = app.config.video_muted;
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
        let center = ui::filmstrip::centering_offset(viewer.nav.cursor(), window_w);
        viewer.filmstrip_scroll_x = center;
        tasks.push(iced::widget::operation::scroll_to(
            ui::filmstrip::filmstrip_id(),
            iced::widget::scrollable::AbsoluteOffset { x: center, y: 0.0 },
        ));
        tasks.extend(fire_visible_thumbs(&pipeline, viewer, window_w));
    }

    if app.config.show_info {
        tasks.push(fire_exif(app));
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

    if crate::video::is_video(&path) {
        // First-frame grab via FFmpeg, which needs a real file on disk.
        if !matches!(viewer.source, Source::Fs) {
            return Task::none();
        }
        viewer.in_flight_thumbs.insert(path.clone());
        let disk = pipeline.disk();
        let probe = async move {
            tokio::task::spawn_blocking(move || {
                let container = path
                    .parent()
                    .unwrap_or(std::path::Path::new(""))
                    .to_path_buf();
                let name = path.file_name().unwrap_or_default().to_owned();
                if let Some(disk) = &disk
                    && let Some(hit) = disk.load(&container, &name)
                {
                    return (path, Ok(hit));
                }
                match crate::video::first_frame_thumb(&path, crate::media::THUMB_DIM) {
                    Some(thumb) => {
                        if let Some(disk) = &disk {
                            disk.store(&container, &name, &thumb, None, 0);
                        }
                        (path, Ok(thumb))
                    }
                    None => (path, Err(MediaError::Unsupported)),
                }
            })
            .await
            .expect("video thumb task panicked")
        };
        return Task::perform(probe, move |(path, result)| Message::ThumbLoaded {
            path,
            urgency,
            result: result.map(|data| Thumb {
                handle: Handle::from_rgba(data.width, data.height, data.pixels),
                size: (data.width, data.height),
                original_size: data.original_size,
            }),
        });
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
    if viewer.cache.contains(&path)
        || viewer.anim_player.has_cached(&path)
        || viewer.in_flight.contains(&path)
        // Videos never go through the image pipeline. Reading a multi-GB
        // file into memory to fail decoding would be a disaster.
        || crate::video::is_video(&path)
    {
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
                    .map(|allocation| LoadedMedia::Static {
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
        Ok(DecodedMedia::Animated(anim)) => {
            // Frames allocate at display time, only the thumb needs a
            // handle here.
            let thumb = anim.thumbnail.as_ref().map(|t| Thumb {
                handle: Handle::from_rgba(t.width, t.height, t.pixels.clone()),
                size: (t.width, t.height),
                original_size: t.original_size,
            });
            Task::done(Message::MediaLoaded {
                path: path.clone(),
                result: Ok(LoadedMedia::Animated { anim, thumb }),
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
    viewer
        .nav
        .peek_around(depth)
        .into_iter()
        .map(|p| fire_load(pipeline, viewer, p, Lane::Prefetch))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::validate_rename;

    #[test]
    fn validate_rename_rejects_bad_input() {
        assert!(validate_rename("").is_err());
        assert!(validate_rename("   ").is_err());
        assert!(validate_rename("a/b.png").is_err());
        assert!(validate_rename("a?.png").is_err());
        assert!(validate_rename("noextension").is_err());
        assert!(validate_rename("file.txt").is_err());
    }

    #[test]
    fn validate_rename_accepts_supported_names() {
        assert_eq!(validate_rename(" photo.png ").unwrap(), "photo.png");
        assert_eq!(validate_rename("IMG_1234.JPG").unwrap(), "IMG_1234.JPG");
    }
}
