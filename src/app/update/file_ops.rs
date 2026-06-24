use std::path::PathBuf;
use std::time::Duration;

use iced::Task;
use iced::widget::image::Handle;

use crate::components::modal::VideoResume;
use crate::components::toasts::ToastKind;
use crate::media::pipeline::Pipeline;

use super::push_toast;
use crate::app::{App, Message, ModalMessage};

/// The current file, if file operations are allowed on it: requires a
/// filesystem source and read-only mode off. Refusals return the toast
/// task explaining why.
pub(crate) fn file_op_target(app: &mut App) -> Result<PathBuf, Task<Message>> {
    let Some(viewer) = app.viewer() else {
        return Err(Task::none());
    };
    if !viewer.is_fs() {
        return Err(push_toast(
            app,
            ToastKind::Info,
            "Archive entries can't be modified".into(),
        ));
    }
    if app.config.read_only {
        return Err(push_toast(
            app,
            ToastKind::Info,
            "Read-only mode is on".into(),
        ));
    }
    Ok(app
        .viewer()
        .map(|v| v.nav.current().to_path_buf())
        .unwrap_or_default())
}

/// Move a file to the recycle bin, off-thread. `resume` restores a video that
/// was torn down to free its handle, should the delete fail.
pub(crate) fn fire_delete(
    app: &mut App,
    path: PathBuf,
    resume: Option<VideoResume>,
) -> Task<Message> {
    app.modal = None;
    let target = path.clone();
    Task::perform(trash_with_retry(target), move |result| {
        Message::Modal(ModalMessage::DeleteFinished(path, result, resume))
    })
}

/// Recycle a file, retrying briefly: the video decoder can keep it open for a
/// moment on Windows after its session drops.
async fn trash_with_retry(path: PathBuf) -> Result<(), String> {
    let mut err = String::new();
    for attempt in 0..5 {
        let p = path.clone();
        let result = tokio::task::spawn_blocking(move || trash::delete(&p))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));
        match result {
            Ok(()) => return Ok(()),
            Err(e) => err = e,
        }
        if attempt < 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Err(err)
}

/// Put the displayed image on the clipboard as bitmap data.
pub(crate) fn copy_bitmap(handle: &Handle) -> Result<(), String> {
    let Handle::Rgba {
        width,
        height,
        pixels,
        ..
    } = handle
    else {
        return Err("no pixel data available".into());
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: *width as usize,
            height: *height as usize,
            bytes: std::borrow::Cow::Borrowed(pixels),
        })
        .map_err(|e| e.to_string())
}

/// Put raw RGBA pixels on the clipboard as bitmap data. Used for video,
/// whose current frame is converted from YUV on demand.
pub(crate) fn copy_rgba_bitmap(width: u32, height: u32, pixels: Vec<u8>) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Owned(pixels),
        })
        .map_err(|e| e.to_string())
}

/// Drop a deleted/renamed file's entry from the persistent thumbnail
/// store so the thumbnail can't outlive the file.
pub(crate) fn purge_disk_thumb(pipeline: &Pipeline, path: &std::path::Path) -> Task<Message> {
    let Some(disk) = pipeline.disk() else {
        return Task::none();
    };
    let container = path
        .parent()
        .unwrap_or(std::path::Path::new(""))
        .to_path_buf();
    let name = path.file_name().unwrap_or_default().to_owned();
    Task::future(async move {
        let _ = tokio::task::spawn_blocking(move || disk.remove(&container, &name)).await;
    })
    .discard()
}

/// Validate a rename input: non-empty, no path/invalid characters, and a
/// supported image extension (anything else would vanish from the list).
pub(crate) fn validate_rename(input: &str) -> Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("Name can't be empty".into());
    }
    if name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']) {
        return Err("Name contains invalid characters".into());
    }
    let supported = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(crate::config::AppConfig::is_supported_extension);
    if !supported {
        return Err("Name must keep a supported image extension".into());
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::validate_rename;

    #[test]
    fn validate_rename_rejects_bad_input() {
        assert!(validate_rename("").is_err());
        assert!(validate_rename("   ").is_err());
        assert!(validate_rename("a/b.png").is_err());
        assert!(validate_rename("a?.png").is_err());
        assert!(validate_rename("noextension").is_err());
        assert!(validate_rename("file.txt").is_err());
    }

    #[test]
    fn validate_rename_accepts_supported_names() {
        assert_eq!(validate_rename(" photo.png ").unwrap(), "photo.png");
        assert_eq!(validate_rename("IMG_1234.JPG").unwrap(), "IMG_1234.JPG");
    }
}
