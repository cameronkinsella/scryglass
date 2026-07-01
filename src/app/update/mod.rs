//! Update function: handles messages, mutates state, fires async tasks.
//!
//! Navigation NEVER blocks: every keypress moves the cursor immediately.
//! A cache hit displays instantly. A miss keeps the previous image on
//! screen and fires a cancellable load. Whatever load finishes for the
//! path under the cursor wins ("latest wins" by path equality).

pub(super) mod file_ops;
pub(crate) mod media;
pub(super) mod media_tasks;
pub(super) mod navigation;
pub(crate) mod open;
pub(super) mod settings;
pub(crate) mod video_flow;
pub(crate) mod window;
#[cfg(target_os = "windows")]
mod working_set;

use std::path::PathBuf;

use iced::Task;

use crate::components::toasts::{Message as ToastMessage, Toast, ToastKind};
use crate::config::AppConfig;

pub(crate) use file_ops::{
    copy_bitmap, copy_rgba_bitmap, file_op_target, fire_delete, purge_disk_thumb, validate_rename,
};
pub(crate) use media_tasks::{
    fire_exif, fire_load, fire_prefetch, fire_rotate, fire_thumbnailer, fire_tiles, run_jobs,
    run_jobs_at, set_prefetch_scaler, settle_tiles, show_loaded, show_placeholder,
    try_start_shared_anim,
};
pub(crate) use navigation::open_path;
pub(crate) use navigation::{
    complete_navigation, fire_resort, navigate, open_viewer, resolve_pending_nav, scrub_to,
};
pub(crate) use settings::{probe_disk_cache_size, save_config};

use super::message::{
    is_background_message, is_context_menu_message, is_menu_message, is_modal_blocked,
};
use super::state::Direction;
use super::{App, Envelope, Message, Shared, Window};

/// Where a navigation lands: one step in a direction, or an absolute index.
pub(crate) enum NavTarget {
    Delta(Direction),
    Index(usize),
}

/// Daemon update: route a per-window message to its window, or handle a
/// window-lifecycle event, then let the store reconcile any image whose demand a
/// dropped lease lowered (navigation, decay, window close).
pub fn update(app: &mut App, envelope: Envelope) -> Task<Envelope> {
    let task = route(app, envelope);
    let store = pump_store(app);
    // A focus/minimize change may have moved the whole app into (or out of) the
    // background, so re-evaluate the working-set trim.
    #[cfg(target_os = "windows")]
    let trim = working_set::reconcile(app);
    #[cfg(not(target_os = "windows"))]
    let trim = Task::none();
    Task::batch([task, store, trim])
}

fn route(app: &mut App, envelope: Envelope) -> Task<Envelope> {
    match envelope {
        Envelope::Win(id, message) => {
            let Some(win) = app.windows.get_mut(&id) else {
                // The window closed mid-flight, but a store completion still
                // owns shared state: dropping it would leave the entry's
                // pending mark set, wedging the image for every other window.
                return orphaned_media(app, message);
            };
            let vp_before = win.viewport_size;
            let window_before = win.window_size;
            let shows_image = super::measure::displays_image(&message);
            let task = dispatch(win, &mut app.shared, message);
            // Remeasure the image area when an image lands on screen or a chrome toggle
            // moves the viewport without a resize. Not during a resize: the calibrated
            // estimate already tracks the true area there, and running the layout
            // operation every resize frame fights iced's live resize (a visible pulse).
            let toggled = win.viewport_size != vp_before && win.window_size == window_before;
            let task = if shows_image || toggled {
                Task::batch([task, super::measure::image_area(win)])
            } else {
                task
            };
            Envelope::wrap(id, task)
        }
        // Replay maximize/fullscreen now the window exists, not at open where it
        // races creation. Use the config: the window's own flag was reset by the
        // placement query that runs first.
        Envelope::Opened(id) => super::boot::replay_window_state(
            id,
            app.shared.config.window_maximized,
            app.shared.config.window_fullscreen,
        ),
        Envelope::Closed(id) => {
            app.windows.remove(&id);
            // A daemon keeps running with no windows, so exit once the last one
            // is gone. iced::exit() does not terminate when the last window
            // closed while minimized (the idle winit loop never processes it),
            // so the process would linger holding the IPC socket. Exit directly.
            if app.windows.is_empty() {
                std::process::exit(0);
            }
            Task::none()
        }
        Envelope::Forwarded(path) => open_new_window(app, path),
        // A live config edit reparsed cleanly. Ignore one equal to the in-memory
        // config (the app's own save tripping the watcher). Otherwise apply it.
        Envelope::ConfigReloaded(config) => {
            if *config != app.shared.config {
                return apply_config(app, *config);
            }
            Task::none()
        }
        Envelope::ConfigInvalid => config_invalid_toast(app),
        #[cfg(target_os = "windows")]
        Envelope::TrimWorkingSet(generation) => working_set::on_timer(app, generation),
    }
}

/// Adopt a hand-edited config across the whole app: re-theme (read live from the
/// config at render), recompute every viewport (chrome/zoom-mode changes shift
/// it), and let the decay tiers pick up the new values on their next pass. Window
/// geometry only takes effect on the next window.
fn apply_config(app: &mut App, config: AppConfig) -> Task<Envelope> {
    app.shared.pipeline.set_ram_budget(
        config
            .resource
            .large_image_ram_budget
            .resolve(crate::config::total_system_ram()),
    );
    set_prefetch_scaler(config.resource.prefetch_scaler);
    app.shared
        .pipeline
        .set_prefetch_parallelism(config.resource.prefetch_parallelism);
    app.shared.config = config;
    let mut tasks = Vec::new();
    for (id, win) in app.windows.iter_mut() {
        super::recalc_viewport(win, &app.shared);
        // A chrome or zoom-mode change moves the placement, like a resize.
        tasks.push(Envelope::wrap(*id, settle_tiles(win)));
    }
    Task::batch(tasks)
}

/// Warn that a live config edit no longer parses, on the focused window (or any
/// window). The current settings stay in effect.
fn config_invalid_toast(app: &mut App) -> Task<Envelope> {
    let target = app
        .windows
        .iter()
        .find(|(_, w)| w.focused)
        .map(|(id, _)| *id)
        .or_else(|| app.windows.keys().next().copied());
    let Some(id) = target else {
        return Task::none();
    };
    let Some(win) = app.windows.get_mut(&id) else {
        return Task::none();
    };
    Envelope::wrap(
        id,
        push_toast(
            win,
            &mut app.shared,
            ToastKind::Error,
            "config.toml has a syntax error; keeping the current settings.".into(),
        ),
    )
}

/// Drain the store's drop-fed dirty queue and run whatever re-mint a lowered
/// demand calls for (re-uploading a now-smaller texture as a higher tier is
/// released). Sync demotes and evictions already happened on the lease drop. This
/// fires only the async re-mints, which touch the shared cell, not any window's
/// display, so they route through any live window. O(keys dirtied), never a scan.
fn pump_store(app: &mut App) -> Task<Envelope> {
    // Reconcile dropped animation leases: free a GIF's shared frames once its last
    // holder let go. This pump only ever frees (demand falls on a drop, never rises),
    // so it yields no jobs to run. Draining it is enough.
    let _ = app.shared.anim_store.pump();
    let outcome = app.shared.store.pump();
    run_shared_jobs(app, outcome)
}

/// Apply a store completion whose window no longer exists. The shared cell,
/// tier, and pending mark move exactly as they would have. Only the closed
/// window's display effects are dropped.
fn orphaned_media(app: &mut App, message: Message) -> Task<Envelope> {
    // An archive-video extraction that outlives its window wrote a real temp
    // file. Wrap it in a guard so it is deleted, like the navigated-away path.
    if let Message::VideoControls(crate::components::video_controls::Message::Extracted {
        result: Ok(temp),
        ..
    }) = message
    {
        drop(crate::video::TempFileGuard::new(temp));
        return Task::none();
    }
    let Message::Media(message) = message else {
        return Task::none();
    };
    let outcome = match message {
        media::Message::Decoded { key, ram, .. } => app.shared.store.on_decoded(key, *ram),
        media::Message::TextureReady { key, tier, texture } => {
            app.shared.store.on_minted(key, tier, texture)
        }
        media::Message::MintFailed { key } => app.shared.store.on_mint_failed(&key),
        media::Message::DecodeFailed { key, err, .. } => app
            .shared
            .store
            .on_decode_failed(&key, matches!(err, crate::media::MediaError::Cancelled)),
        // Nobody leases the frames. The still store forgets the key like the
        // live handler does. Tile settles heal through their claim TTL.
        media::Message::AnimDecoded { key, .. } => {
            app.shared.store.abandon(&key);
            return Task::none();
        }
        _ => return Task::none(),
    };
    run_shared_jobs(app, outcome)
}

/// Run store jobs that belong to no particular window through any live one.
fn run_shared_jobs(app: &mut App, outcome: crate::media::store::StoreOutcome) -> Task<Envelope> {
    if outcome.jobs.is_empty() {
        return Task::none();
    }
    let target = app
        .windows
        .iter()
        .find(|(_, w)| w.viewer().is_some())
        .map(|(id, w)| (*id, w.viewport_size));
    let Some((id, view)) = target else {
        return Task::none();
    };
    let pipeline = app.shared.pipeline.clone();
    let task = run_jobs(
        outcome.jobs,
        &pipeline,
        crate::media::pipeline::Lane::Prefetch,
        view,
    );
    Envelope::wrap(id, task)
}

/// Open a new window for a forwarded launch at the last-saved size (the OS
/// places it). A forward with no path opens an empty window.
fn open_new_window(app: &mut App, path: Option<PathBuf>) -> Task<Envelope> {
    let (id, opened) = iced::window::open(super::boot::window_settings(&app.shared.config));
    let mut win = super::boot::new_window(id, &app.shared.config);
    super::recalc_viewport(&mut win, &app.shared);
    let open = match path {
        Some(path) => {
            win.opening_since = Some(iced::time::Instant::now());
            Envelope::wrap(id, open_path(path))
        }
        None => Task::none(),
    };
    app.windows.insert(id, win);
    // A forwarded window must come to the front: open it, then steal focus once
    // it exists, so a later launch never opens behind the current window.
    Task::batch([
        opened
            .map(Envelope::Opened)
            .chain(iced::window::gain_focus(id)),
        open,
    ])
}

/// Handle a message for one window: auto-dismiss transient UI, then dispatch
/// to the component that owns it.
fn dispatch(win: &mut Window, shared: &mut Shared, message: Message) -> Task<Message> {
    // Auto-dismiss any open dropdown when the user interacts outside the menu,
    // but never on a background event like the minimize poll.
    if win.open_menu.is_some() && !is_menu_message(&message) && !is_background_message(&message) {
        win.open_menu = None;
    }

    if win.context_menu_pos.is_some()
        && !is_context_menu_message(&message)
        && !is_background_message(&message)
    {
        win.context_menu_pos = None;
    }

    // A modal dialog owns the keyboard: hotkey actions go inert so keys the
    // text input doesn't capture never navigate, delete, or nudge the video.
    if win.modal.is_some() && is_modal_blocked(&message) {
        return Task::none();
    }

    match message {
        Message::Open(message) => open::update(win, shared, message),
        Message::Media(message) => media::update(win, shared, message),
        Message::Viewer(message) => crate::components::viewer::update(win, shared, message),
        Message::Toolbar(message) => crate::components::toolbar::update(win, shared, message),
        Message::NavSlider(message) => crate::components::nav_slider::update(win, shared, message),
        Message::Filmstrip(message) => crate::components::filmstrip::update(win, shared, message),
        Message::Modal(message) => crate::components::modal::update(win, shared, message),
        Message::Settings(message) => crate::components::settings::update(win, shared, message),
        Message::ContextMenu(message) => {
            crate::components::context_menu::update(win, shared, message)
        }
        Message::VideoControls(message) => {
            crate::components::video_controls::update(win, shared, message)
        }
        Message::Window(message) => window::update(win, shared, message),
        Message::Toast(message) => crate::components::toasts::update(win, shared, message),
        Message::Anim(message) => media::update_anim(win, shared, message),
    }
}
/// Show a transient notification that dismisses itself after a few seconds.
pub(crate) fn push_toast(
    win: &mut Window,
    _shared: &mut Shared,
    kind: ToastKind,
    text: String,
) -> Task<Message> {
    let id = win.next_toast_id;
    win.next_toast_id += 1;
    win.toasts.push(Toast { id, kind, text });
    Task::perform(
        tokio::time::sleep(std::time::Duration::from_secs(4)),
        move |_| Message::Toast(ToastMessage::Dismiss(id)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{empty_app, into_app, viewing_app};
    use crate::app::{Modal, ViewerMessage};
    use crate::components::toasts::ToastKind;

    // push_toast schedules its auto-dismiss with a tokio timer, which needs
    // a runtime in scope even though the returned Task is dropped here.
    #[tokio::test]
    async fn push_toast_appends_and_increments_the_id() {
        let mut app = empty_app();
        let _ = push_toast(
            &mut app.window,
            &mut app.shared,
            ToastKind::Info,
            "hello".into(),
        );
        assert_eq!(app.window.toasts.len(), 1);
        assert_eq!(app.window.toasts[0].text, "hello");
        assert_eq!(app.window.next_toast_id, 1);
        let _ = push_toast(
            &mut app.window,
            &mut app.shared,
            ToastKind::Error,
            "oops".into(),
        );
        assert_eq!(app.window.next_toast_id, 2);
        assert_ne!(app.window.toasts[0].id, app.window.toasts[1].id);
    }

    #[test]
    fn a_cache_miss_defers_navigation_instead_of_dropping_it() {
        let (mut app, id) = into_app(viewing_app(&["a.png", "b.png"], 0));
        let _ = update(
            &mut app,
            Envelope::Win(id, Message::Viewer(ViewerMessage::Next)),
        );
        // b.png is not cached, so the move is held, not lost: the screen
        // must never go empty during navigation.
        assert!(app.windows[&id].viewer().unwrap().pending_nav.is_some());
    }

    #[test]
    fn a_modal_makes_viewer_navigation_inert() {
        let mut t = viewing_app(&["a.png", "b.png"], 0);
        t.window.modal = Some(Modal::Settings);
        let (mut app, id) = into_app(t);
        let _ = update(
            &mut app,
            Envelope::Win(id, Message::Viewer(ViewerMessage::Next)),
        );
        assert!(app.windows[&id].viewer().unwrap().pending_nav.is_none());
    }

    #[test]
    fn a_store_completion_survives_its_window_closing() {
        use crate::media::store::{ImageKey, RamImage, Tier};

        let (mut app, id) = into_app(viewing_app(&["a.png"], 0));
        let source = crate::media::pipeline::Source::Fs;
        let key = ImageKey::new(&source, std::path::Path::new("a.png"));
        // Another window still leases the image while the closing window's
        // upload is in flight (pending is set).
        let (lease, _) = app
            .shared
            .store
            .request(key.clone(), "a.png".into(), source, Tier::Full);
        let _ = app.shared.store.on_decoded(
            key.clone(),
            RamImage {
                handle: iced::widget::image::Handle::from_rgba(2, 2, vec![0u8; 16]),
                original_size: (2, 2),
                decode_time: None,
            },
        );

        // The upload's window closes before its completion processes.
        let closed = app.windows.remove(&id).unwrap();
        let _ = update(
            &mut app,
            Envelope::Win(
                id,
                Message::Media(media::Message::TextureReady {
                    key: key.clone(),
                    tier: Tier::Full,
                    texture: crate::ui::image_surface::test_keepalive(),
                }),
            ),
        );

        // The completion reached the shared store anyway: the survivor's
        // lease reads the texture and the entry is not wedged pending.
        assert!(lease.texture().is_some());
        assert_eq!(app.shared.store.tier(&key), Tier::Full);
        drop(closed);
    }

    #[test]
    fn a_bare_relaunch_opens_an_empty_window() {
        let (mut app, id) = into_app(empty_app());
        let _ = update(&mut app, Envelope::Forwarded(None));
        assert_eq!(app.windows.len(), 2);
        let new_id = app.windows.keys().find(|k| **k != id).copied().unwrap();
        let win = &app.windows[&new_id];
        assert!(matches!(win.session, crate::app::state::Session::Empty));
        // No path to load, so it shows the drop prompt, not the opening spinner.
        assert!(win.opening_since.is_none());
    }

    #[test]
    fn a_config_reload_applies_new_settings_across_windows() {
        use crate::config::ThemeChoice;
        let (mut app, _id) = into_app(viewing_app(&["a.png"], 0));
        let mut edited = app.shared.config.clone();
        edited.theme = ThemeChoice::Light;
        edited.prefetch_depth = 7;

        let _ = update(&mut app, Envelope::ConfigReloaded(Box::new(edited)));

        assert_eq!(app.shared.config.theme, ThemeChoice::Light);
        assert_eq!(app.shared.config.prefetch_depth, 7);
    }

    #[test]
    fn a_forwarded_path_opens_a_loading_window() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("photo.png");
        std::fs::write(&file, b"").unwrap();
        let (mut app, id) = into_app(empty_app());
        let _ = update(&mut app, Envelope::Forwarded(Some(file)));
        assert_eq!(app.windows.len(), 2);
        let new_id = app.windows.keys().find(|k| **k != id).copied().unwrap();
        assert!(app.windows[&new_id].opening_since.is_some());
    }

    #[test]
    fn new_windows_take_the_saved_size_not_the_open_window() {
        let (mut app, id) = into_app(empty_app());
        // The open window is large (e.g. it was maximized) but the saved size
        // is smaller. A new window uses the saved size, never copies the big one.
        app.windows.get_mut(&id).unwrap().window_size = iced::Size::new(3000.0, 2000.0);
        app.shared.config.window_width = 800.0;
        app.shared.config.window_height = 600.0;
        let _ = update(&mut app, Envelope::Forwarded(None));
        let new_id = app.windows.keys().find(|k| **k != id).copied().unwrap();
        assert_eq!(
            app.windows[&new_id].window_size,
            iced::Size::new(800.0, 600.0)
        );
    }
}
