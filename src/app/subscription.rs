//! Subscriptions: translate runtime events (keyboard, mouse, file drops,
//! window) into messages, and drive the timers for spinners, animation,
//! and video frame pacing.

use std::path::PathBuf;
use std::time::Duration;

use iced::futures::{SinkExt, Stream};
use iced::keyboard::Key;
use iced::keyboard::key::Named;
use iced::{Event, Subscription, event, keyboard, mouse, window};

use crate::anim::AnimMessage;
use crate::media::pipeline::Source;

use super::shortcuts;
use super::{
    App, MediaMessage, Message, OpenMessage, VideoControlsMessage, ViewerMessage, WindowMessage,
};

/// Subscription: keyboard/mouse/file-drop events, GIF animation ticks,
/// and a redraw driver while the loading spinner is visible.
pub fn subscription(app: &App) -> Subscription<Message> {
    let mut subs = vec![
        event::listen_with(handle_event),
        // Close requests route through update() so config saves first.
        iced::window::close_requests().map(|id| Message::Window(WindowMessage::CloseRequested(id))),
    ];

    // The opening spinner runs before any viewer exists.
    if app.opening_since.is_some() {
        subs.push(
            iced::time::every(Duration::from_millis(33))
                .map(|_| Message::Media(MediaMessage::SpinnerTick)),
        );
    }

    if let Some(viewer) = app.viewer() {
        if viewer.pending_since.is_some() && app.opening_since.is_none() {
            subs.push(
                iced::time::every(Duration::from_millis(33))
                    .map(|_| Message::Media(MediaMessage::SpinnerTick)),
            );
        }

        if viewer.pending_since.is_none()
            && viewer.anim_player.is_animating()
            && let Some(delay) = viewer.anim_player.current_delay()
        {
            subs.push(iced::time::every(delay).map(|_| Message::Anim(AnimMessage::Tick)));
        }

        // Video pacing: pull frames due for display ~60×/s while a
        // session is active (paused sessions still need control redraws).
        if viewer.video.is_some() {
            subs.push(
                iced::time::every(Duration::from_millis(16))
                    .map(|_| Message::VideoControls(VideoControlsMessage::Tick)),
            );
        }

        // A held edge press has no OS key-repeat, so drive it here. Leaving
        // the strip clears edge_held, so this stops the moment the cursor does.
        if viewer.edge_held.is_some() {
            subs.push(
                iced::time::every(crate::app::EDGE_NAV_REPEAT)
                    .map(|_| Message::Viewer(ViewerMessage::EdgeRepeat)),
            );
        }

        // Keep the file list in sync with the folder on disk.
        if matches!(viewer.source, Source::Fs)
            && let Some(dir) = viewer.nav.current().parent()
        {
            subs.push(watch_dir(dir.to_path_buf()));
        }
    }

    Subscription::batch(subs)
}

/// Subscribe to filesystem changes in `dir`, keyed by the directory so opening
/// a different folder restarts the watch.
fn watch_dir(dir: PathBuf) -> Subscription<Message> {
    Subscription::run_with(dir, watch_stream)
}

/// The watcher runs on its own thread. Events coalesce over a quiet window so a
/// bulk change triggers a single refresh.
// `run_with` hands the builder `&D`, so the parameter has to be `&PathBuf`.
#[allow(clippy::ptr_arg)]
fn watch_stream(dir: &PathBuf) -> impl Stream<Item = Message> + use<> {
    use notify::{RecursiveMode, Watcher};

    let dir = dir.clone();
    iced::stream::channel(
        4,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let mut watcher =
                match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if res.is_ok() {
                        let _ = tx.send(());
                    }
                }) {
                    Ok(watcher) => watcher,
                    Err(_) => return,
                };
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            while rx.recv().await.is_some() {
                // Drain the burst until the folder goes quiet, then refresh once.
                while let Ok(again) =
                    tokio::time::timeout(Duration::from_millis(300), rx.recv()).await
                {
                    if again.is_none() {
                        return;
                    }
                }
                if output
                    .send(Message::Open(OpenMessage::DirectoryChanged(dir.clone())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        },
    )
}

/// Returns true if the key is a forward navigation key (ArrowRight or D).
fn is_forward_key(key: &Key) -> bool {
    matches!(key, Key::Named(Named::ArrowRight))
        || matches!(key, Key::Character(c) if c.as_ref() == "d")
}

/// Returns true if the key is a backward navigation key (ArrowLeft or A).
fn is_backward_key(key: &Key) -> bool {
    matches!(key, Key::Named(Named::ArrowLeft))
        || matches!(key, Key::Character(c) if c.as_ref() == "a")
}

/// Whether a focused widget captured this keyboard event, so the global
/// shortcut table should leave it alone (the rename input owns its keys).
fn shortcut_suppressed(event: &Event, status: event::Status) -> bool {
    status == event::Status::Captured && matches!(event, Event::Keyboard(_))
}

fn handle_event(event: Event, status: event::Status, _id: window::Id) -> Option<Message> {
    if shortcut_suppressed(&event, status) {
        return None;
    }
    match &event {
        // --- Keyboard: initial press ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_forward_key(key) => Some(Message::Viewer(ViewerMessage::Next)),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: false, ..
        }) if is_backward_key(key) => Some(Message::Viewer(ViewerMessage::Prev)),

        // --- Keyboard: everything else goes through the shortcut table.
        // `modified_key` includes shift effects, so "?" and "R" arrive
        // as themselves rather than "/" and "r".
        Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key,
            modifiers,
            repeat: false,
            ..
        }) => shortcuts::map_press(modified_key, *modifiers),

        // --- Keyboard: OS key-repeat ---
        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_forward_key(key) => Some(Message::Viewer(ViewerMessage::NextRepeat)),

        Event::Keyboard(keyboard::Event::KeyPressed {
            key, repeat: true, ..
        }) if is_backward_key(key) => Some(Message::Viewer(ViewerMessage::PrevRepeat)),

        // --- Keyboard: key released ---
        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_forward_key(key) => {
            Some(Message::Viewer(ViewerMessage::NextReleased))
        }

        Event::Keyboard(keyboard::Event::KeyReleased { key, .. }) if is_backward_key(key) => {
            Some(Message::Viewer(ViewerMessage::PrevReleased))
        }

        // --- Mouse: back/forward buttons (single navigation, no hold) ---
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Forward)) => {
            Some(Message::Viewer(ViewerMessage::Next))
        }
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Back)) => {
            Some(Message::Viewer(ViewerMessage::Prev))
        }

        // --- Mouse: cursor moved (for drag panning) ---
        Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Some(Message::Viewer(ViewerMessage::DragMove(*position)))
        }
        Event::Mouse(mouse::Event::CursorLeft) => Some(Message::Viewer(ViewerMessage::CursorLeft)),

        // --- Mouse: left button released (end drag) ---
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::Viewer(ViewerMessage::DragEnd))
        }

        // --- File drop ---
        Event::Window(window::Event::FileDropped(path)) => {
            Some(Message::Open(OpenMessage::FileDropped(path.clone())))
        }

        // --- Window resized ---
        Event::Window(window::Event::Resized(size)) => {
            Some(Message::Window(WindowMessage::Resized(*size)))
        }

        // --- Window lost focus: close the zoom pop-up ---
        Event::Window(window::Event::Unfocused) => {
            Some(Message::Viewer(ViewerMessage::CloseZoomSlider))
        }

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> window::Id {
        window::Id::unique()
    }

    fn handle(event: Event) -> Option<Message> {
        handle_event(event, event::Status::Ignored, id())
    }

    #[test]
    fn forward_keys_are_arrow_right_and_d() {
        assert!(is_forward_key(&Key::Named(Named::ArrowRight)));
        assert!(is_forward_key(&Key::Character("d".into())));
        assert!(!is_forward_key(&Key::Named(Named::ArrowLeft)));
        assert!(!is_forward_key(&Key::Character("x".into())));
    }

    #[test]
    fn backward_keys_are_arrow_left_and_a() {
        assert!(is_backward_key(&Key::Named(Named::ArrowLeft)));
        assert!(is_backward_key(&Key::Character("a".into())));
        assert!(!is_backward_key(&Key::Named(Named::ArrowRight)));
    }

    #[test]
    fn mouse_forward_and_back_buttons_navigate() {
        let fwd = handle(Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Forward,
        )));
        assert!(matches!(fwd, Some(Message::Viewer(ViewerMessage::Next))));
        let back = handle(Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Back,
        )));
        assert!(matches!(back, Some(Message::Viewer(ViewerMessage::Prev))));
    }

    #[test]
    fn cursor_motion_drives_drag_panning() {
        let moved = handle(Event::Mouse(mouse::Event::CursorMoved {
            position: iced::Point::new(3.0, 4.0),
        }));
        assert!(matches!(
            moved,
            Some(Message::Viewer(ViewerMessage::DragMove(_)))
        ));
        let left = handle(Event::Mouse(mouse::Event::CursorLeft));
        assert!(matches!(
            left,
            Some(Message::Viewer(ViewerMessage::CursorLeft))
        ));
        let up = handle(Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        )));
        assert!(matches!(up, Some(Message::Viewer(ViewerMessage::DragEnd))));
    }

    #[test]
    fn window_resize_maps_to_a_resize_message() {
        let msg = handle(Event::Window(window::Event::Resized(iced::Size::new(
            640.0, 480.0,
        ))));
        assert!(matches!(
            msg,
            Some(Message::Window(WindowMessage::Resized(_)))
        ));
    }

    #[test]
    fn file_drop_maps_to_an_open_message() {
        let msg = handle(Event::Window(window::Event::FileDropped(
            std::path::PathBuf::from("x.png"),
        )));
        assert!(matches!(
            msg,
            Some(Message::Open(OpenMessage::FileDropped(_)))
        ));
    }

    #[test]
    fn unhandled_events_produce_no_message() {
        let msg = handle(Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Middle,
        )));
        assert!(msg.is_none());
    }

    #[test]
    fn captured_keyboard_events_are_suppressed() {
        let key = Event::Keyboard(keyboard::Event::ModifiersChanged(
            keyboard::Modifiers::default(),
        ));
        assert!(shortcut_suppressed(&key, event::Status::Captured));
        assert!(!shortcut_suppressed(&key, event::Status::Ignored));
        // Mouse events pass through even when captured.
        let mouse = Event::Mouse(mouse::Event::CursorLeft);
        assert!(!shortcut_suppressed(&mouse, event::Status::Captured));
    }
}
