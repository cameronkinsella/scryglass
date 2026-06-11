//! Keyboard shortcut mapping: one tested table instead of scattered
//! match arms.
//!
//! Navigation keys (arrows, A/D) are NOT mapped here: their press/repeat/
//! release lifecycle drives hold-to-scroll and stays in the event handler.

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

use crate::app::Message;

/// Map a (non-repeat) key press to a message.
pub fn map_press(key: &Key, modifiers: Modifiers) -> Option<Message> {
    let ctrl = modifiers.command();

    match key {
        Key::Named(Named::Escape) => Some(Message::Escape),
        Key::Named(Named::Home) => Some(Message::First),
        Key::Named(Named::End) => Some(Message::Last),
        Key::Named(Named::F11) => Some(Message::ToggleFullscreen),
        Key::Named(Named::Delete) => Some(Message::RequestDelete),
        Key::Named(Named::F2) => Some(Message::RequestRename),
        Key::Named(Named::Enter) => Some(Message::ModalSubmit),
        // Video transport (no-ops when nothing is playing).
        Key::Named(Named::Space) => Some(Message::VideoPlayPause),
        Key::Named(Named::ArrowUp) => Some(Message::VideoNudgeVolume(0.1)),
        Key::Named(Named::ArrowDown) => Some(Message::VideoNudgeVolume(-0.1)),

        Key::Character(c) => match c.as_str() {
            "f" | "F" if !ctrl => Some(Message::ToggleFullscreen),
            "i" | "I" if !ctrl => Some(Message::ToggleInfo),
            "r" if !ctrl => Some(Message::Rotate(1)),
            "R" if !ctrl => Some(Message::Rotate(3)),
            "?" => Some(Message::ToggleHelp),
            "+" | "=" => Some(Message::ZoomStep(1)),
            "-" => Some(Message::ZoomStep(-1)),
            "0" if ctrl => Some(Message::ResetZoom),
            "1" if ctrl => Some(Message::ZoomActual),
            "m" | "M" if !ctrl => Some(Message::VideoToggleMute),
            "j" | "J" if !ctrl => Some(Message::VideoSeekBy(-10.0)),
            "l" | "L" if !ctrl => Some(Message::VideoSeekBy(10.0)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::Modifiers;

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    #[test]
    fn fullscreen_keys() {
        assert!(matches!(
            map_press(&Key::Named(Named::F11), Modifiers::default()),
            Some(Message::ToggleFullscreen)
        ));
        assert!(matches!(
            map_press(&ch("f"), Modifiers::default()),
            Some(Message::ToggleFullscreen)
        ));
    }

    #[test]
    fn zoom_keys() {
        assert!(matches!(
            map_press(&ch("+"), Modifiers::default()),
            Some(Message::ZoomStep(1))
        ));
        assert!(matches!(
            map_press(&ch("-"), Modifiers::default()),
            Some(Message::ZoomStep(-1))
        ));
        assert!(matches!(
            map_press(&ch("0"), Modifiers::CTRL),
            Some(Message::ResetZoom)
        ));
        assert!(matches!(
            map_press(&ch("1"), Modifiers::CTRL),
            Some(Message::ZoomActual)
        ));
        // Bare digits do nothing.
        assert!(map_press(&ch("0"), Modifiers::default()).is_none());
    }

    #[test]
    fn home_end_navigate() {
        assert!(matches!(
            map_press(&Key::Named(Named::Home), Modifiers::default()),
            Some(Message::First)
        ));
        assert!(matches!(
            map_press(&Key::Named(Named::End), Modifiers::default()),
            Some(Message::Last)
        ));
    }

    #[test]
    fn rotation_distinguishes_shift() {
        assert!(matches!(
            map_press(&ch("r"), Modifiers::default()),
            Some(Message::Rotate(1))
        ));
        assert!(matches!(
            map_press(&ch("R"), Modifiers::SHIFT),
            Some(Message::Rotate(3))
        ));
    }

    #[test]
    fn help_key() {
        assert!(matches!(
            map_press(&ch("?"), Modifiers::SHIFT),
            Some(Message::ToggleHelp)
        ));
    }

    #[test]
    fn info_panel_key() {
        assert!(matches!(
            map_press(&ch("i"), Modifiers::default()),
            Some(Message::ToggleInfo)
        ));
    }

    #[test]
    fn escape_maps() {
        assert!(matches!(
            map_press(&Key::Named(Named::Escape), Modifiers::default()),
            Some(Message::Escape)
        ));
    }

    #[test]
    fn unmapped_keys_are_none() {
        assert!(map_press(&ch("q"), Modifiers::default()).is_none());
        assert!(map_press(&ch("f"), Modifiers::CTRL).is_none());
    }
}
