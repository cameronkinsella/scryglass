//! Update function: handles messages, mutates state, fires async tasks.
//!
//! Navigation NEVER blocks: every keypress moves the cursor immediately.
//! A cache hit displays instantly. A miss keeps the previous image on
//! screen and fires a cancellable load. Whatever load finishes for the
//! path under the cursor wins ("latest wins" by path equality).

mod file_ops;
mod media_tasks;
mod navigation;
mod settings;
mod video_flow;

use iced::Task;
use iced::time::Instant;

use crate::anim::AnimMessage;
use crate::config::{AppConfig, ThemeChoice, ZoomMode};
use crate::media::MediaError;
use crate::media::pipeline::{Lane, Source, ThumbUrgency};
use crate::nav::Nav;
use crate::ui;
use crate::ui::toast::{Toast, ToastKind};
use crate::ui::toolbar::OpenMenu;

use file_ops::{copy_bitmap, file_op_target, fire_delete, purge_disk_thumb, validate_rename};
use media_tasks::{
    fire_exif, fire_load, fire_rotate, fire_thumbnailer, fire_visible_thumbs, show_loaded,
    show_placeholder,
};
pub(super) use navigation::open_path;
use navigation::{
    complete_navigation, fire_resort, navigate, open_viewer, resolve_pending_nav, scrub_to,
};
use settings::{probe_disk_cache_size, save_config};

use super::Modal;
use super::message::{is_context_menu_message, is_menu_message, is_viewer_interaction};
use super::state::{Direction, DisplayedImage, DragState, LoadedMedia, Session, SliderDrag};
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
        Message::VideoTick => video_flow::tick(app),
        Message::VideoExtracted { entry, result } => video_flow::extracted(app, entry, result),
        Message::VideoFrame { path, image } => video_flow::frame(app, path, image),
        Message::VideoPlayPause => video_flow::play_pause(app),
        Message::VideoSeekDrag(secs) => video_flow::seek_drag(app, secs),
        Message::VideoSeekRelease => video_flow::seek_release(app),
        Message::VideoSeekBy(delta) => video_flow::seek_by(app, delta),
        Message::VideoSetVolume(volume) => video_flow::set_volume(app, volume),
        Message::VideoNudgeVolume(delta) => video_flow::nudge_volume(app, delta),
        Message::VideoToggleMute => video_flow::toggle_mute(app),
        Message::VideoToggleLoop => video_flow::toggle_loop(app),

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
