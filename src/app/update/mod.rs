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

use iced::Task;

use crate::components::toasts::{Message as ToastMessage, Toast, ToastKind};

pub(crate) use file_ops::{
    copy_bitmap, copy_rgba_bitmap, file_op_target, fire_delete, purge_disk_thumb, validate_rename,
};
pub(crate) use media_tasks::{
    fire_exif, fire_load, fire_rotate, fire_thumbnailer, show_loaded, show_placeholder,
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
/// window-lifecycle event.
pub fn update(app: &mut App, envelope: Envelope) -> Task<Envelope> {
    match envelope {
        Envelope::Win(id, message) => {
            let Some(win) = app.windows.get_mut(&id) else {
                return Task::none();
            };
            Envelope::wrap(id, dispatch(win, &mut app.shared, message))
        }
        Envelope::Opened(_id) => Task::none(),
        Envelope::Closed(id) => {
            app.windows.remove(&id);
            // A daemon keeps running with no windows, so exit once the last
            // one is gone.
            if app.windows.is_empty() {
                iced::exit()
            } else {
                Task::none()
            }
        }
    }
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
}
