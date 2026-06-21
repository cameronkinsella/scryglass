//! Keyboard shortcut mapping: one tested table instead of scattered
//! match arms.
//!
//! Navigation keys (arrows, A/D) are NOT mapped here: their press/repeat/
//! release lifecycle drives hold-to-scroll and stays in the event handler.

use iced::keyboard::key::Named;
use iced::keyboard::{Key, Modifiers};

use crate::app::{
    ContextMenuMessage, Message, ModalMessage, OpenMessage, ToolbarMessage, VideoMessage,
    ViewerMessage,
};

/// Map a (non-repeat) key press to a message.
pub fn map_press(key: &Key, modifiers: Modifiers) -> Option<Message> {
    let ctrl = modifiers.command();
    let shift = modifiers.shift();

    match key {
        Key::Named(Named::Escape) => Some(Message::Viewer(ViewerMessage::Escape)),
        Key::Named(Named::Home) => Some(Message::Viewer(ViewerMessage::First)),
        Key::Named(Named::End) => Some(Message::Viewer(ViewerMessage::Last)),
        Key::Named(Named::F11) => Some(Message::Viewer(ViewerMessage::ToggleFullscreen)),
        Key::Named(Named::Delete) => Some(Message::Modal(ModalMessage::RequestDelete)),
        Key::Named(Named::F2) => Some(Message::Modal(ModalMessage::RequestRename)),
        Key::Named(Named::Enter) => Some(Message::Modal(ModalMessage::Submit)),
        // Video transport (no-ops when nothing is playing).
        Key::Named(Named::Space) => Some(Message::VideoControls(VideoMessage::PlayPause)),
        Key::Named(Named::ArrowUp) => Some(Message::VideoControls(VideoMessage::NudgeVolume(0.1))),
        Key::Named(Named::ArrowDown) => {
            Some(Message::VideoControls(VideoMessage::NudgeVolume(-0.1)))
        }

        Key::Character(c) => match c.as_str() {
            "f" | "F" if !ctrl => Some(Message::Viewer(ViewerMessage::ToggleFullscreen)),
            "i" | "I" if !ctrl => Some(Message::Viewer(ViewerMessage::ToggleInfo)),
            "t" | "T" if !ctrl => Some(Message::Toolbar(ToolbarMessage::ToggleToolbar)),
            "r" if !ctrl => Some(Message::Viewer(ViewerMessage::Rotate(1))),
            "R" if !ctrl => Some(Message::Viewer(ViewerMessage::Rotate(3))),
            "?" => Some(Message::Viewer(ViewerMessage::ToggleHelp)),
            "+" | "=" => Some(Message::Viewer(ViewerMessage::ZoomStep(1))),
            "-" => Some(Message::Viewer(ViewerMessage::ZoomStep(-1))),
            "0" if ctrl => Some(Message::Viewer(ViewerMessage::ResetZoom)),
            "1" if ctrl => Some(Message::Viewer(ViewerMessage::ZoomActual)),
            // Shift+Ctrl+C arrives as the shifted character "C".
            "c" | "C" if ctrl && shift => {
                Some(Message::ContextMenu(ContextMenuMessage::CopyFilePath))
            }
            "c" if ctrl => Some(Message::ContextMenu(ContextMenuMessage::CopyImage)),
            "o" | "O" if ctrl => Some(Message::Open(OpenMessage::OpenFile)),
            "m" | "M" if !ctrl => Some(Message::VideoControls(VideoMessage::ToggleMute)),
            "j" | "J" if !ctrl => Some(Message::VideoControls(VideoMessage::SeekBy(-10.0))),
            "l" | "L" if !ctrl => Some(Message::VideoControls(VideoMessage::SeekBy(10.0))),
            "." if !ctrl => Some(Message::VideoControls(VideoMessage::StepFrame(1))),
            "," if !ctrl => Some(Message::VideoControls(VideoMessage::StepFrame(-1))),
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

    /// The platform command modifier, matching `Modifiers::command()`:
    /// Cmd on macOS, Ctrl everywhere else.
    fn cmd() -> Modifiers {
        if cfg!(target_os = "macos") {
            Modifiers::LOGO
        } else {
            Modifiers::CTRL
        }
    }

    #[test]
    fn fullscreen_keys() {
        assert!(matches!(
            map_press(&Key::Named(Named::F11), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::ToggleFullscreen))
        ));
        assert!(matches!(
            map_press(&ch("f"), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::ToggleFullscreen))
        ));
    }

    #[test]
    fn zoom_keys() {
        assert!(matches!(
            map_press(&ch("+"), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::ZoomStep(1)))
        ));
        assert!(matches!(
            map_press(&ch("-"), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::ZoomStep(-1)))
        ));
        assert!(matches!(
            map_press(&ch("0"), cmd()),
            Some(Message::Viewer(ViewerMessage::ResetZoom))
        ));
        assert!(matches!(
            map_press(&ch("1"), cmd()),
            Some(Message::Viewer(ViewerMessage::ZoomActual))
        ));
        // Bare digits do nothing.
        assert!(map_press(&ch("0"), Modifiers::default()).is_none());
    }

    #[test]
    fn home_end_navigate() {
        assert!(matches!(
            map_press(&Key::Named(Named::Home), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::First))
        ));
        assert!(matches!(
            map_press(&Key::Named(Named::End), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::Last))
        ));
    }

    #[test]
    fn rotation_distinguishes_shift() {
        assert!(matches!(
            map_press(&ch("r"), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::Rotate(1)))
        ));
        assert!(matches!(
            map_press(&ch("R"), Modifiers::SHIFT),
            Some(Message::Viewer(ViewerMessage::Rotate(3)))
        ));
    }

    #[test]
    fn help_key() {
        assert!(matches!(
            map_press(&ch("?"), Modifiers::SHIFT),
            Some(Message::Viewer(ViewerMessage::ToggleHelp))
        ));
    }

    #[test]
    fn info_panel_key() {
        assert!(matches!(
            map_press(&ch("i"), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::ToggleInfo))
        ));
    }

    #[test]
    fn escape_maps() {
        assert!(matches!(
            map_press(&Key::Named(Named::Escape), Modifiers::default()),
            Some(Message::Viewer(ViewerMessage::Escape))
        ));
    }

    #[test]
    fn clipboard_and_open_shortcuts() {
        assert!(matches!(
            map_press(&ch("c"), cmd()),
            Some(Message::ContextMenu(ContextMenuMessage::CopyImage))
        ));
        assert!(matches!(
            map_press(&ch("C"), cmd() | Modifiers::SHIFT),
            Some(Message::ContextMenu(ContextMenuMessage::CopyFilePath))
        ));
        assert!(matches!(
            map_press(&ch("o"), cmd()),
            Some(Message::Open(OpenMessage::OpenFile))
        ));
        assert!(matches!(
            map_press(&ch("t"), Modifiers::default()),
            Some(Message::Toolbar(ToolbarMessage::ToggleToolbar))
        ));
        // Bare C must not copy.
        assert!(map_press(&ch("c"), Modifiers::default()).is_none());
    }

    #[test]
    fn frame_step_keys() {
        assert!(matches!(
            map_press(&ch("."), Modifiers::default()),
            Some(Message::VideoControls(VideoMessage::StepFrame(1)))
        ));
        assert!(matches!(
            map_press(&ch(","), Modifiers::default()),
            Some(Message::VideoControls(VideoMessage::StepFrame(-1)))
        ));
    }

    #[test]
    fn unmapped_keys_are_none() {
        assert!(map_press(&ch("q"), Modifiers::default()).is_none());
        assert!(map_press(&ch("f"), cmd()).is_none());
    }
}
