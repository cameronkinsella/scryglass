use super::update::{media, open, window};
use crate::anim::AnimMessage;
use crate::components::{
    context_menu, filmstrip, modal, nav_slider, settings, toasts, toolbar, video_controls, viewer,
};

#[derive(Debug, Clone)]
pub enum Message {
    Open(open::Message),
    Media(media::Message),
    Viewer(viewer::Message),
    Toolbar(toolbar::Message),
    NavSlider(nav_slider::Message),
    Filmstrip(filmstrip::Message),
    Modal(modal::Message),
    Settings(settings::Message),
    ContextMenu(context_menu::Message),
    VideoControls(video_controls::Message),
    Window(window::Message),
    Toast(toasts::Message),
    Anim(AnimMessage),
}

macro_rules! impl_message_from {
    ($($module:ident => $variant:ident),+ $(,)?) => {
        $(
            impl From<$module::Message> for Message {
                fn from(message: $module::Message) -> Self {
                    Self::$variant(message)
                }
            }
        )+
    };
}

impl_message_from! {
    open => Open,
    media => Media,
    viewer => Viewer,
    toolbar => Toolbar,
    nav_slider => NavSlider,
    filmstrip => Filmstrip,
    modal => Modal,
    settings => Settings,
    context_menu => ContextMenu,
    video_controls => VideoControls,
    window => Window,
    toasts => Toast,
}

/// Messages a modal dialog suppresses so the keyboard stays in its text input:
/// the viewer, video, and toolbar hotkey actions (but not the passive video
/// tick, nor the input's own RenameInput/Submit, which must still flow).
pub fn is_modal_blocked(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Viewer(
            viewer::Message::Next
                | viewer::Message::Prev
                | viewer::Message::NextRepeat
                | viewer::Message::PrevRepeat
                | viewer::Message::First
                | viewer::Message::Last
                | viewer::Message::ZoomStep(_)
                | viewer::Message::ZoomActual
                | viewer::Message::ResetZoom
                | viewer::Message::Rotate(_)
                | viewer::Message::ToggleFullscreen
                | viewer::Message::ToggleInfo
                | viewer::Message::ToggleHelp
        ) | Message::VideoControls(
            video_controls::Message::PlayPause
                | video_controls::Message::NudgeVolume(_)
                | video_controls::Message::ToggleMute
                | video_controls::Message::SeekBy(_)
                | video_controls::Message::StepFrame(_)
        ) | Message::Toolbar(toolbar::Message::ToggleToolbar)
            | Message::Modal(modal::Message::RequestDelete | modal::Message::RequestRename)
    )
}

pub fn is_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Toolbar(_)
            | Message::Settings(_)
            | Message::ContextMenu(_)
            | Message::Open(
                open::Message::OpenFile
                    | open::Message::CloseFile
                    | open::Message::Quit
                    | open::Message::DirectoryScanned(_, _, _)
                    | open::Message::DirectoryChanged(_)
                    | open::Message::DirectoryRescanned(_, _)
                    | open::Message::ArchiveScanned(_, _)
                    | open::Message::FileDialogResult(_)
            )
            | Message::Media(
                media::Message::Loaded { .. }
                    | media::Message::ThumbLoaded { .. }
                    | media::Message::FileSizeProbed(_, _)
                    | media::Message::Resorted(_)
                    | media::Message::ExifLoaded(_, _)
                    | media::Message::ViewRotated { .. }
                    | media::Message::SpinnerTick
            )
            | Message::Toast(toasts::Message::Dismiss(_))
            | Message::Filmstrip(filmstrip::Message::Scrolled(_))
            | Message::VideoControls(
                video_controls::Message::Tick | video_controls::Message::Extracted { .. }
            )
            | Message::Anim(_)
            | Message::Viewer(
                viewer::Message::DragMove(_)
                    | viewer::Message::CursorLeft
                    | viewer::Message::DragEnd
                    | viewer::Message::NextReleased
                    | viewer::Message::PrevReleased
                    // Layout toggles live in the Layout menu, so flipping
                    // them leaves it open like its toolbar siblings.
                    | viewer::Message::ToggleInfo
                    | viewer::Message::ToggleCheckerboard
            )
            | Message::Window(window::Message::Resized(_))
            | Message::Modal(modal::Message::RequestDelete | modal::Message::RequestRename)
    )
}

pub fn is_context_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ContextMenu(_)
            | Message::Toolbar(toolbar::Message::ToggleToolbar)
            | Message::Modal(modal::Message::RequestDelete | modal::Message::RequestRename)
            | Message::Open(
                open::Message::DirectoryScanned(_, _, _)
                    | open::Message::DirectoryChanged(_)
                    | open::Message::DirectoryRescanned(_, _)
                    | open::Message::ArchiveScanned(_, _)
                    | open::Message::FileDialogResult(_)
            )
            | Message::Media(
                media::Message::Loaded { .. }
                    | media::Message::ThumbLoaded { .. }
                    | media::Message::FileSizeProbed(_, _)
                    | media::Message::Resorted(_)
                    | media::Message::ExifLoaded(_, _)
                    | media::Message::ViewRotated { .. }
                    | media::Message::SpinnerTick
            )
            | Message::Toast(toasts::Message::Dismiss(_))
            | Message::Filmstrip(filmstrip::Message::Scrolled(_))
            | Message::VideoControls(
                video_controls::Message::Tick | video_controls::Message::Extracted { .. }
            )
            | Message::Anim(_)
            | Message::Viewer(
                viewer::Message::DragMove(_)
                    | viewer::Message::CursorLeft
                    | viewer::Message::NextReleased
                    | viewer::Message::PrevReleased
            )
            | Message::Window(window::Message::Resized(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_blocks_hotkey_actions() {
        assert!(is_modal_blocked(&viewer::Message::Next.into()));
        assert!(is_modal_blocked(&viewer::Message::ZoomStep(1).into()));
        assert!(is_modal_blocked(
            &video_controls::Message::NudgeVolume(0.1).into()
        ));
        assert!(is_modal_blocked(&toolbar::Message::ToggleToolbar.into()));
        assert!(is_modal_blocked(&modal::Message::RequestRename.into()));
        // Typing into the field and the passive video tick still flow.
        assert!(!is_modal_blocked(
            &modal::Message::RenameInput("photo.png".to_string()).into()
        ));
        assert!(!is_modal_blocked(&modal::Message::Submit.into()));
        assert!(!is_modal_blocked(&video_controls::Message::Tick.into()));
    }

    #[test]
    fn passive_messages_do_not_close_menus() {
        assert!(is_menu_message(&media::Message::SpinnerTick.into()));
        assert!(is_menu_message(
            &window::Message::Resized(iced::Size::new(800.0, 600.0)).into()
        ));
        assert!(is_menu_message(&toasts::Message::Dismiss(7).into()));
        assert!(is_menu_message(
            &open::Message::FileDialogResult(None).into()
        ));
    }

    #[test]
    fn active_viewer_messages_close_menus() {
        assert!(!is_menu_message(&viewer::Message::Next.into()));
        assert!(!is_menu_message(&viewer::Message::Prev.into()));
        assert!(!is_menu_message(&viewer::Message::ScrollZoom(1.0).into()));
        assert!(!is_menu_message(&viewer::Message::ZoomActual.into()));
    }

    #[test]
    fn layout_toggles_keep_the_menu_open() {
        assert!(is_menu_message(&viewer::Message::ToggleInfo.into()));
        assert!(is_menu_message(&viewer::Message::ToggleCheckerboard.into()));
    }

    #[test]
    fn context_menu_keeps_its_own_flow_and_passive_updates() {
        assert!(is_context_menu_message(
            &context_menu::Message::CopyFilename.into()
        ));
        assert!(is_context_menu_message(
            &context_menu::Message::OpenImageLocation.into()
        ));
        assert!(is_context_menu_message(&media::Message::SpinnerTick.into()));
        assert!(is_context_menu_message(
            &window::Message::Resized(iced::Size::new(800.0, 600.0)).into()
        ));
    }

    #[test]
    fn non_context_actions_close_context_menu() {
        assert!(!is_context_menu_message(&viewer::Message::Next.into()));
        assert!(!is_context_menu_message(
            &toolbar::Message::ToggleZoomMenu.into()
        ));
        assert!(!is_context_menu_message(&settings::Message::Open.into()));
    }
}
