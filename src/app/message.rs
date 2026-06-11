//! The message enum driving all state transitions, plus menu-dismissal
//! classification helpers.

use std::path::PathBuf;
use std::sync::Arc;

use iced::Size;

use crate::anim::AnimMessage;
use crate::config::{SortKey, ZoomMode};
use crate::media::MediaError;
use crate::media::archive::ArchiveIndex;
use crate::media::pipeline::ThumbUrgency;

use super::state::{CachedImage, LoadedMedia, Thumb};

#[derive(Debug, Clone)]
pub enum Message {
    FileDropped(PathBuf),
    DirectoryScanned(PathBuf, Result<Vec<PathBuf>, String>),
    /// An archive was opened and its image entries listed.
    ArchiveScanned(PathBuf, Result<Arc<ArchiveIndex>, String>),
    /// A pipeline load finished (current or prefetch), successfully or not.
    MediaLoaded {
        path: PathBuf,
        result: Result<LoadedMedia, MediaError>,
    },
    /// A thumbnail job finished (EXIF probe or background decode).
    ThumbLoaded {
        path: PathBuf,
        urgency: ThumbUrgency,
        result: Result<Thumb, MediaError>,
    },
    /// Async file-size probe completed for the given path.
    FileSizeProbed(PathBuf, u64),
    /// Redraw driver for the loading spinner while a load is pending.
    SpinnerTick,
    /// A toast's display time elapsed.
    DismissToast(u64),
    /// Wrapped GIF player message.
    Anim(AnimMessage),
    /// Navigate forward (initial press).
    Next,
    /// Navigate backward (initial press).
    Prev,
    /// Jump to the first image.
    First,
    /// Jump to the last image.
    Last,
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
    /// Toggle the Sort dropdown menu.
    ToggleSortMenu,
    /// Change the sort key (re-sorts the open folder).
    SetSortKey(SortKey),
    /// Flip ascending/descending.
    ToggleSortDirection,
    /// Background re-sort finished.
    Resorted(Vec<PathBuf>),
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
    /// Keyboard zoom step about the viewport center (+1 in, -1 out).
    ZoomStep(i8),
    /// Set zoom to exactly 100%.
    ZoomActual,
    /// Double-click: reset zoom to auto/opening state.
    ResetZoom,
    /// Toggle borderless fullscreen.
    ToggleFullscreen,
    /// Toggle the info panel (file details + EXIF).
    ToggleInfo,
    /// Rotate the view by quarter turns clockwise (non-destructive).
    Rotate(u8),
    /// A rotated texture for the current image is ready.
    ViewRotated {
        path: PathBuf,
        /// Total turns baked into this texture.
        baked: u8,
        image: CachedImage,
    },
    /// Toggle the checkerboard backdrop behind transparent images.
    ToggleCheckerboard,
    /// Toggle the keyboard shortcut help overlay.
    ToggleHelp,
    /// EXIF probe finished for the given file.
    ExifLoaded(PathBuf, Vec<(String, String)>),
    /// Escape: leaves fullscreen, otherwise dismisses any open overlay.
    Escape,
    /// Mouse pressed on image area, begin drag.
    DragStart,
    /// Mouse moved during drag.
    DragMove(iced::Point),
    /// Mouse released, end drag.
    DragEnd,
    /// Window resized.
    WindowResized(Size),
    /// Slider drag moved to an image index (also fired on click).
    SliderChanged(usize),
    /// Slider drag released, commit the target.
    SliderReleased,
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
    /// The filmstrip's scroll offset changed (user or programmatic).
    FilmstripScrolled(f32),
    /// Toggle toolbar visibility.
    ToggleToolbar,
    /// Switch between the dark and light theme.
    ToggleTheme,
    /// Toggle nearest-neighbor sampling when zoomed past 100%.
    ToggleCrispPixels,
    /// Show the context menu at the cursor position.
    ShowContextMenu,
    /// Dismiss the context menu.
    DismissContextMenu,
    /// Delete the current file (asks for confirmation unless disabled).
    RequestDelete,
    /// Confirmed: move the current file to the recycle bin.
    ConfirmDeleteNow,
    /// Trash operation finished.
    DeleteFinished(PathBuf, Result<(), String>),
    /// Open the rename dialog for the current file.
    RequestRename,
    /// Rename dialog text changed.
    RenameInput(String),
    /// Validate and execute the rename.
    CommitRename,
    /// Filesystem rename finished.
    RenameFinished(PathBuf, PathBuf, Result<(), String>),
    /// Enter pressed while a modal is open, submit it.
    ModalSubmit,
    /// Close the open modal without acting.
    ModalCancel,
    /// Open the settings dialog.
    OpenSettings,
    /// Disk thumbnail cache size probe finished.
    DiskCacheSize(u64),
    /// Wipe the disk thumbnail cache.
    ClearDiskThumbs,
    /// Settings: prefetch depth changed.
    SetPrefetchDepth(usize),
    /// Settings: image cache budget changed (MB).
    SetCacheBudget(usize),
    /// Settings: toggle pure-viewer mode.
    ToggleReadOnly,
    /// Settings: toggle the delete confirmation.
    ToggleConfirmDelete,
    /// Settings: toggle the persistent thumbnail store (applies on restart).
    ToggleDiskThumbs,
    /// Bitmap clipboard copy finished.
    CopyImageFinished(Result<(), String>),
    /// The OS asked to close the window, save config first.
    CloseRequested(iced::window::Id),
    /// Copy the current image to clipboard (as bitmap).
    CopyImage,
    /// Copy the current file to clipboard (as a file-list entry).
    CopyFile,
    /// Copy the full file path to clipboard.
    CopyFilePath,
    /// Copy just the filename to clipboard.
    CopyFilename,
    /// Open the folder containing the image in the native file explorer.
    OpenImageLocation,
    /// Open the native file properties dialog on the Details tab.
    ImageProperties,
}

/// Keyboard-driven viewer interactions that must go inert while a modal
/// dialog is open because the global event subscription would otherwise leak
/// keystrokes typed into a text input (e.g. "a"/"d") into navigation.
pub fn is_viewer_interaction(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Next
            | Message::Prev
            | Message::NextRepeat
            | Message::PrevRepeat
            | Message::First
            | Message::Last
            | Message::ZoomStep(_)
            | Message::ZoomActual
            | Message::ResetZoom
            | Message::Rotate(_)
            | Message::ToggleFullscreen
            | Message::ToggleInfo
            | Message::ToggleHelp
            | Message::RequestDelete
            | Message::RequestRename
    )
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
            | Message::ToggleSortMenu
            | Message::SetSortKey(_)
            | Message::ToggleSortDirection
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
            | Message::ToggleCrispPixels
            | Message::ToggleInfo
            | Message::ToggleCheckerboard
            | Message::OpenSettings
            // Settings dialog interactions keep the dialog open:
            | Message::SetPrefetchDepth(_)
            | Message::SetCacheBudget(_)
            | Message::ToggleReadOnly
            | Message::ToggleConfirmDelete
            | Message::ToggleDiskThumbs
            | Message::ClearDiskThumbs
            | Message::DiskCacheSize(_)
            // Context menu messages:
            | Message::ShowContextMenu
            | Message::DismissContextMenu
            | Message::CopyImage
            | Message::CopyFile
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            | Message::RequestDelete
            | Message::RequestRename
            // Passive events that shouldn't dismiss menus:
            | Message::DragMove(_)
            | Message::DragEnd
            | Message::WindowResized(_)
            | Message::MediaLoaded { .. }
            | Message::ThumbLoaded { .. }
            | Message::FileSizeProbed(_, _)
            | Message::SpinnerTick
            | Message::DismissToast(_)
            | Message::FilmstripScrolled(_)
            | Message::Resorted(_)
            | Message::ExifLoaded(_, _)
            | Message::ViewRotated { .. }
            | Message::Anim(_)
            | Message::DirectoryScanned(_, _)
            | Message::ArchiveScanned(_, _)
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
            | Message::CopyFile
            | Message::CopyFilePath
            | Message::CopyFilename
            | Message::OpenImageLocation
            | Message::ImageProperties
            | Message::RequestDelete
            | Message::RequestRename
            | Message::ToggleToolbar
            // Passive events:
            | Message::DragMove(_)
            | Message::WindowResized(_)
            | Message::MediaLoaded { .. }
            | Message::ThumbLoaded { .. }
            | Message::FileSizeProbed(_, _)
            | Message::SpinnerTick
            | Message::DismissToast(_)
            | Message::FilmstripScrolled(_)
            | Message::Resorted(_)
            | Message::ExifLoaded(_, _)
            | Message::ViewRotated { .. }
            | Message::Anim(_)
            | Message::DirectoryScanned(_, _)
            | Message::ArchiveScanned(_, _)
            | Message::FileDialogResult(_)
            | Message::NextReleased
            | Message::PrevReleased
    )
}
