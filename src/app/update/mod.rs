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

use std::path::PathBuf;

use iced::Task;

use crate::components::toasts::{Message as ToastMessage, Toast, ToastKind};

pub(crate) use file_ops::{
    copy_bitmap, copy_rgba_bitmap, file_op_target, fire_delete, purge_disk_thumb, validate_rename,
};
pub(crate) use media_tasks::{
    fire_exif, fire_load, fire_prefetch, fire_promote, fire_restore_textures, fire_rotate,
    fire_thumbnailer, show_loaded, show_placeholder,
};
pub(crate) use navigation::open_path;
pub(crate) use navigation::{
    complete_navigation, fire_resort, navigate, open_viewer, resolve_pending_nav, scrub_to,
};
pub(crate) use settings::{probe_disk_cache_size, save_config};

use super::message::{is_context_menu_message, is_menu_message, is_modal_blocked};
use super::state::Direction;
use super::{App, Envelope, Message, Shared, Window};

/// Where a navigation lands: one step in a direction, or an absolute index.
pub(crate) enum NavTarget {
    Delta(Direction),
    Index(usize),
}

/// Daemon update: route a per-window message to its window, or handle a
/// window-lifecycle event, then re-split cache budgets if the window count
/// changed.
pub fn update(app: &mut App, envelope: Envelope) -> Task<Envelope> {
    let task = route(app, envelope);
    rebalance_budgets(app);
    task
}

fn route(app: &mut App, envelope: Envelope) -> Task<Envelope> {
    match envelope {
        Envelope::Win(id, message) => {
            let Some(win) = app.windows.get_mut(&id) else {
                return Task::none();
            };
            Envelope::wrap(id, dispatch(win, &mut app.shared, message))
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
    }
}

/// Divide the image and thumbnail budgets evenly across the windows that hold a
/// viewer, so total cache memory stays bounded however many windows are open.
/// Cheap per message (a count plus a budget compare); only a viewer whose share
/// actually changed pays for an eviction pass.
fn rebalance_budgets(app: &mut App) {
    let share = app
        .windows
        .values()
        .filter(|w| w.viewer().is_some())
        .count()
        .max(1);
    let cache_each = app.shared.config.cache_budget_mb * 1024 * 1024 / share;
    let thumb_each = super::state::THUMB_BUDGET_BYTES / share;
    let depth = app.shared.config.prefetch_depth;
    for win in app.windows.values_mut() {
        if let Some(viewer) = win.viewer_mut() {
            if viewer.cache.budget() == cache_each {
                continue;
            }
            viewer.cache.set_budget(cache_each);
            viewer.thumbs.set_budget(thumb_each);
            let pinned = viewer.pinned_paths(depth);
            viewer.cache.evict_over_budget(&pinned);
            viewer.thumbs.evict_over_budget(&pinned);
        }
    }
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
    Task::batch([opened.map(Envelope::Opened), open])
}

/// Handle a message for one window: auto-dismiss transient UI, then dispatch
/// to the component that owns it.
fn dispatch(win: &mut Window, shared: &mut Shared, message: Message) -> Task<Message> {
    // Auto-dismiss any open dropdown when the user interacts outside the menu.
    if win.open_menu.is_some() && !is_menu_message(&message) {
        win.open_menu = None;
    }

    // Auto-dismiss context menu on any non-context-menu interaction.
    if win.context_menu_pos.is_some() && !is_context_menu_message(&message) {
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
    fn cache_budget_is_split_across_viewer_windows() {
        let (mut app, id1) = into_app(viewing_app(&["a.png"], 0));
        let full = app.shared.config.cache_budget_mb * 1024 * 1024;
        // One viewer holds the whole budget.
        rebalance_budgets(&mut app);
        assert_eq!(app.windows[&id1].viewer().unwrap().cache.budget(), full);

        // A second viewer window halves each window's share.
        let second = viewing_app(&["b.png"], 0).window;
        let id2 = second.id;
        app.windows.insert(id2, second);
        rebalance_budgets(&mut app);
        assert_eq!(app.windows[&id1].viewer().unwrap().cache.budget(), full / 2);
        assert_eq!(app.windows[&id2].viewer().unwrap().cache.budget(), full / 2);

        // Closing one restores the survivor to the whole budget.
        app.windows.remove(&id2);
        rebalance_budgets(&mut app);
        assert_eq!(app.windows[&id1].viewer().unwrap().cache.budget(), full);
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
