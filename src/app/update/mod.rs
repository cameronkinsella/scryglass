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
    fire_exif, fire_load, fire_rotate, fire_thumbnailer, fire_visible_thumbs, show_loaded,
    show_placeholder,
};
pub(crate) use navigation::open_path;
pub(crate) use navigation::{
    complete_navigation, fire_resort, navigate, open_viewer, resolve_pending_nav, scrub_to,
};
pub(crate) use settings::{probe_disk_cache_size, save_config};

use super::message::{is_context_menu_message, is_menu_message, is_viewer_interaction};
use super::state::Direction;
use super::{App, Message};

/// Where a navigation lands: one step in a direction, or an absolute index.
pub(crate) enum NavTarget {
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
        Message::Open(message) => open::update(app, message),
        Message::Media(message) => media::update(app, message),
        Message::Viewer(message) => crate::components::viewer::update(app, message),
        Message::Toolbar(message) => crate::components::toolbar::update(app, message),
        Message::NavSlider(message) => crate::components::nav_slider::update(app, message),
        Message::Filmstrip(message) => crate::components::filmstrip::update(app, message),
        Message::Modal(message) => crate::components::modal::update(app, message),
        Message::Settings(message) => crate::components::settings::update(app, message),
        Message::ContextMenu(message) => crate::components::context_menu::update(app, message),
        Message::VideoControls(message) => crate::components::video_controls::update(app, message),
        Message::Window(message) => window::update(app, message),
        Message::Toast(message) => crate::components::toasts::update(app, message),
        Message::Anim(message) => media::update_anim(app, message),
    }
}
/// Show a transient notification that dismisses itself after a few seconds.
pub(crate) fn push_toast(app: &mut App, kind: ToastKind, text: String) -> Task<Message> {
    let id = app.next_toast_id;
    app.next_toast_id += 1;
    app.toasts.push(Toast { id, kind, text });
    Task::perform(
        tokio::time::sleep(std::time::Duration::from_secs(4)),
        move |_| Message::Toast(ToastMessage::Dismiss(id)),
    )
}
