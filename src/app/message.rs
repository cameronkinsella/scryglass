//! The message enum driving all state transitions, plus menu-dismissal
//! classification helpers.

use std::path::PathBuf;

use iced::Size;

use crate::config::ZoomMode;
use crate::gif::GifMessage;
use crate::media::MediaError;

use super::state::CachedImage;

#[derive(Debug, Clone)]
pub enum Message {
    FileDropped(PathBuf),
    DirectoryScanned(PathBuf, Result<Vec<PathBuf>, String>),
    /// A pipeline load finished (current or prefetch), successfully or not.
    MediaLoaded {
        path: PathBuf,
        result: Result<CachedImage, MediaError>,
    },
    /// Async file-size probe completed for the given path.
    FileSizeProbed(PathBuf, u64),
    /// Wrapped GIF player message.
    Gif(GifMessage),
    /// Navigate forward (initial press).
    Next,
    /// Navigate backward (initial press).
    Prev,
    /// Navigate forward (OS key-repeat).
    NextRepeat,
    /// Navigate backward (OS key-repeat).
    PrevRepeat,
    /// Forward key released.
    NextReleased,
    /// Backward key released.
    PrevReleased,
    /// Toggle the File dropdown menu.
    ToggleFileMenu,
    /// Toggle the Zoom dropdown menu.
    ToggleZoomMenu,
    /// Toggle the Layout dropdown menu.
    ToggleLayoutMenu,
    /// Dismiss any open overlay (click outside menu).
    DismissOverlay,
    /// Open a file via native dialog.
    OpenFile,
    /// File dialog completed.
    FileDialogResult(Option<PathBuf>),
    /// Close the current image (return to empty state).
    CloseFile,
    /// Quit the application.
    Quit,
    /// Set the zoom mode.
    SetZoomMode(ZoomMode),
    /// Scroll wheel zoom, delta lines Y.
    ScrollZoom(f32),
    /// Double-click: reset zoom to auto/opening state.
    ResetZoom,
    /// Mouse pressed on image area, begin drag.
    DragStart,
    /// Mouse moved during drag.
    DragMove(iced::Point),
    /// Mouse released, end drag.
    DragEnd,
    /// Window resized.
    WindowResized(Size),
    /// Slider dragged to an image index.
    SliderChanged(usize),
    /// Filmstrip thumbnail clicked.
    FilmstripClicked(usize),
    /// Toggle filmstrip visibility.
    ToggleFilmstrip,
    /// Toggle slider visibility.
    ToggleSlider,
    /// Toggle footer visibility.
    ToggleFooter,
    /// Vertical scroll over filmstrip, convert to horizontal scroll.
    FilmstripScroll(f32),
    /// Toggle toolbar visibility.
    ToggleToolbar,
    /// Switch between the dark and light theme.
    ToggleTheme,
    /// Show the context menu at the cursor position.
    ShowContextMenu,
    /// Dismiss the context menu.
    DismissContextMenu,
    /// Copy the current image to clipboard (as bitmap).
    CopyImage,
    /// Copy the full file path to clipboard.
    CopyFilePath,
    /// Copy just the filename to clipboard.
    CopyFilename,
    /// Open the folder containing the image in the native file explorer.
    OpenImageLocation,
    /// Open the native file properties dialog on the Details tab.
    ImageProperties,
}

/// Returns true if the message is related to menu interaction
/// (opening/closing menus, selecting menu items, or passive events
/// that shouldn't dismiss menus like cursor moves and window resizes).
pub fn is_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ToggleFileMenu
            | Message::ToggleZoomMenu
            | Message::ToggleLayoutMenu
            | Message::DismissOverlay
            | Message::OpenFile
            | Message::CloseFile
            | Message::Quit
            | Message::SetZoomMode(_)
            | Message::ToggleFilmstrip
            | Message::ToggleSlider
            | Message::ToggleFooter
            | Message::ToggleToolbar
            | Message::ToggleTheme
            // Context menu messages:
            | Message::ShowContextMenu
            | Message::DismissContextMenu
            | Message::CopyImage
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            // Passive events that shouldn't dismiss menus:
            | Message::DragMove(_)
            | Message::DragEnd
            | Message::WindowResized(_)
            | Message::MediaLoaded { .. }
            | Message::FileSizeProbed(_, _)
            | Message::Gif(_)
            | Message::DirectoryScanned(_, _)
            | Message::FileDialogResult(_)
            | Message::NextReleased
            | Message::PrevReleased
    )
}

/// Returns true if the message belongs to the context menu flow.
pub fn is_context_menu_message(msg: &Message) -> bool {
    matches!(
        msg,
        Message::ShowContextMenu
            | Message::DismissContextMenu
            | Message::CopyImage
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            | Message::ToggleToolbar
            // Passive events:
            | Message::DragMove(_)
            | Message::WindowResized(_)
            | Message::MediaLoaded { .. }
            | Message::FileSizeProbed(_, _)
            | Message::Gif(_)
            | Message::DirectoryScanned(_, _)
            | Message::FileDialogResult(_)
            | Message::NextReleased
            | Message::PrevReleased
    )
}
