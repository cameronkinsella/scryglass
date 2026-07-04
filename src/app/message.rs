use iced::Task;

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

/// The daemon-level message. Every per-window [`Message`] is tagged with the
/// window it targets. The rest are window-lifecycle events the runtime feeds
/// back. Component code only ever deals in [`Message`]. The top-level update
/// and view wrap and unwrap the envelope at the boundary.
#[derive(Debug, Clone)]
pub enum Envelope {
    /// A message for the window with this id.
    Win(iced::window::Id, Message),
    /// A window the app requested has finished opening.
    Opened(iced::window::Id),
    /// A window closed. Its state is dropped, and the process exits once the
    /// last one is gone.
    Closed(iced::window::Id),
    /// A later launch forwarded a new window to open: a file path, or None for
    /// a bare relaunch (an empty window).
    Forwarded(Option<std::path::PathBuf>),
    /// The watched config file changed and reparsed to a new, valid config
    /// (boxed to keep the envelope small).
    ConfigReloaded(Box<crate::config::AppConfig>),
    /// The watched config file changed but no longer parses. Keep the current
    /// settings and warn.
    ConfigInvalid,
    /// The working-set trim timer fired for this generation (Windows only). A
    /// background-state change since arming bumps the generation, so a
    /// superseded timer no-ops.
    #[cfg(target_os = "windows")]
    TrimWorkingSet(u64),
}

impl Envelope {
    /// Tag a per-window task with the window it belongs to.
    pub(crate) fn wrap(id: iced::window::Id, task: Task<Message>) -> Task<Envelope> {
        task.map(move |message| Envelope::Win(id, message))
    }
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

/// Window-system events and periodic polls (focus, minimize checks, resize,
/// move, the decay-stage timers). None of these are the user interacting
/// outside an open menu, so they must never dismiss one.
pub fn is_background_message(msg: &Message) -> bool {
    matches!(msg, Message::Window(w) if !matches!(w, window::Message::CloseRequested(_)))
}

/// Async completions and timer ticks: background events that are never the
/// user interacting, so neither an open toolbar menu nor a context menu may
/// dismiss on them (the CheckMinimize-closes-menu bug class). Shared by
/// [`is_menu_message`] and [`is_context_menu_message`], which add only the
/// arms where the two menus genuinely differ.
fn is_passive_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Open(
            open::Message::DirectoryScanned(_, _, _)
                | open::Message::DirectoryChanged(_)
                | open::Message::DirectoryRescanned(_, _)
                | open::Message::ArchiveScanned(_, _)
                | open::Message::FileDialogResult(_)
        ) | Message::Media(
            media::Message::Decoded { .. }
                | media::Message::TextureReady { .. }
                | media::Message::TileReady { .. }
                | media::Message::TilesSettled { .. }
                | media::Message::ExactReady { .. }
                | media::Message::MintFailed { .. }
                | media::Message::DecodeFailed { .. }
                | media::Message::AnimDecoded { .. }
                | media::Message::ThumbLoaded { .. }
                | media::Message::FileSizeProbed(_, _)
                | media::Message::Resorted(_)
                | media::Message::ExifLoaded(_, _)
                | media::Message::ViewRotated { .. }
                | media::Message::PromoteCurrent(_)
                | media::Message::SpinnerTick
        ) | Message::Toast(toasts::Message::Dismiss(_))
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
    )
}

pub fn is_menu_message(msg: &Message) -> bool {
    is_passive_message(msg)
        || matches!(
            msg,
            Message::Toolbar(_)
                | Message::Settings(_)
                | Message::ContextMenu(_)
                | Message::Open(
                    open::Message::OpenFile | open::Message::CloseFile | open::Message::Quit
                )
                | Message::Viewer(
                    // The dropdown opens on a left click and every left release
                    // emits DragEnd, so its own opening release must not close
                    // it. The context menu (right-click) deliberately closes on
                    // a left release instead.
                    viewer::Message::DragEnd
                    // Layout toggles live in the Layout menu, so flipping
                    // them leaves it open like its toolbar siblings.
                    | viewer::Message::ToggleInfo
                    | viewer::Message::ToggleCheckerboard
                )
                | Message::Modal(modal::Message::RequestDelete | modal::Message::RequestRename)
        )
}

pub fn is_context_menu_message(msg: &Message) -> bool {
    is_passive_message(msg)
        || matches!(
            msg,
            Message::ContextMenu(_)
                | Message::Toolbar(toolbar::Message::ToggleToolbar)
                | Message::Modal(modal::Message::RequestDelete | modal::Message::RequestRename)
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
        assert!(is_menu_message(&toasts::Message::Dismiss(7).into()));
        assert!(is_menu_message(
            &open::Message::FileDialogResult(None).into()
        ));
    }

    #[test]
    fn background_window_events_do_not_close_menus() {
        // The minimize poll and focus changes must never close a menu (the bug
        // where the periodic CheckMinimize self-closed it).
        assert!(is_background_message(
            &window::Message::CheckMinimize.into()
        ));
        assert!(is_background_message(
            &window::Message::Focused(false).into()
        ));
        assert!(is_background_message(
            &window::Message::Resized(iced::Size::new(800.0, 600.0)).into()
        ));
        assert!(!is_menu_message(&window::Message::CheckMinimize.into()));
        // A genuine close request is not background.
        assert!(!is_background_message(
            &window::Message::CloseRequested(iced::window::Id::unique()).into()
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
    fn passive_completions_flow_through_both_menu_predicates() {
        // The shared passive set feeds both predicates identically, so a
        // background completion never closes either menu.
        for msg in [
            Message::from(media::Message::SpinnerTick),
            Message::from(toasts::Message::Dismiss(1)),
            Message::from(video_controls::Message::Tick),
            Message::from(viewer::Message::CursorLeft),
            Message::from(open::Message::FileDialogResult(None)),
        ] {
            assert!(is_passive_message(&msg));
            assert!(is_menu_message(&msg));
            assert!(is_context_menu_message(&msg));
        }
    }

    #[test]
    fn drag_end_keeps_the_dropdown_but_closes_the_context_menu() {
        // Deliberate asymmetry: the dropdown opens on a left click, so the
        // DragEnd from its own opening release must not close it. A left
        // release anywhere does close the (right-click) context menu.
        let msg: Message = viewer::Message::DragEnd.into();
        assert!(is_menu_message(&msg));
        assert!(!is_context_menu_message(&msg));
    }

    #[test]
    fn file_menu_actions_keep_the_dropdown_but_close_the_context_menu() {
        for msg in [
            Message::from(open::Message::OpenFile),
            Message::from(open::Message::CloseFile),
            Message::from(viewer::Message::ToggleInfo),
        ] {
            assert!(is_menu_message(&msg));
            assert!(!is_context_menu_message(&msg));
        }
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
        // Window-system events are background, not context-menu messages, but
        // still must not close the context menu.
        assert!(is_background_message(
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
