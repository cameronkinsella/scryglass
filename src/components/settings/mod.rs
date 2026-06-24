#[derive(Debug, Clone)]
pub enum Message {
    Open,
    Close,
    ShowHelp,
    DiskCacheSize(u64),
    #[cfg(feature = "disk-thumbs")]
    ClearDiskThumbs,
    SetPrefetchDepth(usize),
    SetCacheBudget(usize),
    ToggleReadOnly,
    ToggleConfirmDelete,
    ToggleMouseNav,
    #[cfg(feature = "disk-thumbs")]
    ToggleDiskThumbs,
    #[cfg(feature = "video")]
    ToggleHardwareDecode,
    ToggleFileAssociations,
    #[cfg(feature = "update-check")]
    CheckForUpdates,
    #[cfg(feature = "update-check")]
    UpdateChecked(crate::update_check::UpdateStatus),
    #[cfg(feature = "update-check")]
    OpenReleasePage(String),
}
use iced::{Element, Task};

use crate::app::update::{probe_disk_cache_size, push_toast, save_config};
use crate::app::{App, Message as AppMessage, Modal};
use crate::components::empty;
use crate::components::toasts::ToastKind;

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    match app.modal {
        Some(Modal::Settings) => widget::settings(
            &app.config,
            app.disk_cache_size,
            app.associations_registered,
            #[cfg(feature = "update-check")]
            app.update_status.as_ref(),
        )
        .map(AppMessage::Settings),
        _ => empty(),
    }
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Open => {
            app.open_menu = None;
            app.modal = Some(Modal::Settings);
            app.disk_cache_size = None;
            app.associations_registered = crate::platform::file_associations_registered();
            probe_disk_cache_size(&app.pipeline)
        }

        Message::Close => {
            app.modal = None;
            // Start fresh next open, so a reopen never shows a stale verdict.
            #[cfg(feature = "update-check")]
            {
                app.update_status = None;
            }
            Task::none()
        }

        Message::ShowHelp => {
            app.modal = None;
            app.help_open = true;
            Task::none()
        }

        Message::ToggleFileAssociations => {
            let result = if app.associations_registered {
                crate::platform::unregister_file_associations().map(|_| false)
            } else {
                crate::platform::register_file_associations().map(|_| true)
            };
            match result {
                Ok(registered) => {
                    app.associations_registered = registered;
                    let note = if registered {
                        "Done. To make scryglass the default, pick it under Windows Settings > Default apps."
                    } else {
                        "scryglass no longer appears in the Open with menu."
                    };
                    push_toast(app, ToastKind::Info, note.into())
                }
                Err(e) => push_toast(app, ToastKind::Error, format!("Couldn't update: {e}")),
            }
        }

        Message::DiskCacheSize(bytes) => {
            app.disk_cache_size = Some(bytes);
            Task::none()
        }

        #[cfg(feature = "disk-thumbs")]
        Message::ClearDiskThumbs => {
            let Some(disk) = app.pipeline.disk() else {
                return Task::none();
            };
            app.disk_cache_size = None;
            let pipeline = app.pipeline.clone();
            Task::batch([
                Task::future(async move {
                    let _ = tokio::task::spawn_blocking(move || disk.clear()).await;
                })
                .then(move |_| probe_disk_cache_size(&pipeline)),
                push_toast(app, ToastKind::Info, "Thumbnail store cleared".into()),
            ])
        }

        Message::SetPrefetchDepth(depth) => {
            app.config.prefetch_depth = depth.clamp(1, 10);
            save_config(app)
        }

        Message::SetCacheBudget(megabytes) => {
            app.config.cache_budget_mb = megabytes.clamp(128, 4096);
            let budget = app.config.cache_budget_mb * 1024 * 1024;
            let depth = app.config.prefetch_depth;
            if let Some(viewer) = app.viewer_mut() {
                viewer.cache.set_budget(budget);
                let pinned = viewer.pinned_paths(depth);
                viewer.cache.evict_over_budget(&pinned);
            }
            save_config(app)
        }

        Message::ToggleReadOnly => {
            app.config.read_only = !app.config.read_only;
            save_config(app)
        }

        Message::ToggleConfirmDelete => {
            app.config.confirm_delete = !app.config.confirm_delete;
            save_config(app)
        }

        Message::ToggleMouseNav => {
            app.config.mouse_nav = !app.config.mouse_nav;
            save_config(app)
        }

        #[cfg(feature = "disk-thumbs")]
        Message::ToggleDiskThumbs => {
            app.config.disk_thumbs = !app.config.disk_thumbs;
            app.pipeline
                .set_disk(crate::media::disk_thumbs::DiskThumbs::create(
                    app.config.disk_thumbs,
                ));
            app.disk_cache_size = None;
            Task::batch([save_config(app), probe_disk_cache_size(&app.pipeline)])
        }

        // Applies to the next video opened.
        #[cfg(feature = "video")]
        Message::ToggleHardwareDecode => {
            app.config.hardware_decode = !app.config.hardware_decode;
            save_config(app)
        }

        #[cfg(feature = "update-check")]
        Message::CheckForUpdates => {
            app.update_status = Some(crate::update_check::UpdateStatus::Checking);
            Task::perform(crate::update_check::fetch_latest(), |status| {
                AppMessage::Settings(Message::UpdateChecked(status))
            })
        }

        #[cfg(feature = "update-check")]
        Message::UpdateChecked(status) => {
            app.update_status = Some(status);
            Task::none()
        }

        #[cfg(feature = "update-check")]
        Message::OpenReleasePage(url) => {
            let _ = open::that(url);
            Task::none()
        }
    }
}
mod widget;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn toggle_read_only_flips_the_flag() {
        let mut app = empty_app();
        let before = app.config.read_only;
        let _ = update(&mut app, Message::ToggleReadOnly);
        assert_eq!(app.config.read_only, !before);
    }

    #[test]
    fn toggle_confirm_delete_flips_the_flag() {
        let mut app = empty_app();
        let before = app.config.confirm_delete;
        let _ = update(&mut app, Message::ToggleConfirmDelete);
        assert_eq!(app.config.confirm_delete, !before);
    }

    #[test]
    fn toggle_mouse_nav_flips_the_flag() {
        let mut app = empty_app();
        let before = app.config.mouse_nav;
        let _ = update(&mut app, Message::ToggleMouseNav);
        assert_eq!(app.config.mouse_nav, !before);
    }

    #[cfg(feature = "video")]
    #[test]
    fn toggle_hardware_decode_flips_the_flag() {
        let mut app = empty_app();
        let before = app.config.hardware_decode;
        let _ = update(&mut app, Message::ToggleHardwareDecode);
        assert_eq!(app.config.hardware_decode, !before);
    }

    #[test]
    fn prefetch_depth_clamps_to_one_through_ten() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::SetPrefetchDepth(0));
        assert_eq!(app.config.prefetch_depth, 1);
        let _ = update(&mut app, Message::SetPrefetchDepth(99));
        assert_eq!(app.config.prefetch_depth, 10);
        let _ = update(&mut app, Message::SetPrefetchDepth(4));
        assert_eq!(app.config.prefetch_depth, 4);
    }

    #[test]
    fn cache_budget_clamps_to_its_range() {
        let mut app = empty_app();
        let _ = update(&mut app, Message::SetCacheBudget(1));
        assert_eq!(app.config.cache_budget_mb, 128);
        let _ = update(&mut app, Message::SetCacheBudget(99_999));
        assert_eq!(app.config.cache_budget_mb, 4096);
        let _ = update(&mut app, Message::SetCacheBudget(512));
        assert_eq!(app.config.cache_budget_mb, 512);
    }

    #[test]
    fn show_help_closes_settings_and_opens_help() {
        let mut app = empty_app();
        app.modal = Some(Modal::Settings);
        let _ = update(&mut app, Message::ShowHelp);
        assert!(app.modal.is_none());
        assert!(app.help_open);
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn update_checked_stores_the_status() {
        use crate::update_check::UpdateStatus;
        let mut app = empty_app();
        let _ = update(&mut app, Message::UpdateChecked(UpdateStatus::UpToDate));
        assert_eq!(app.update_status, Some(UpdateStatus::UpToDate));
    }

    #[cfg(feature = "update-check")]
    #[test]
    fn closing_settings_clears_the_update_status() {
        use crate::update_check::UpdateStatus;
        let mut app = empty_app();
        app.modal = Some(Modal::Settings);
        app.update_status = Some(UpdateStatus::UpToDate);
        let _ = update(&mut app, Message::Close);
        assert!(app.update_status.is_none());
    }
}
