#[derive(Debug, Clone)]
pub enum Message {
    Open,
    Close,
    DiskCacheSize(u64),
    ClearDiskThumbs,
    SetPrefetchDepth(usize),
    SetCacheBudget(usize),
    ToggleReadOnly,
    ToggleConfirmDelete,
    ToggleDiskThumbs,
    ToggleFileAssociations,
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

        Message::ToggleDiskThumbs => {
            app.config.disk_thumbs = !app.config.disk_thumbs;
            app.pipeline
                .set_disk(crate::media::disk_thumbs::DiskThumbs::create(
                    app.config.disk_thumbs,
                ));
            app.disk_cache_size = None;
            Task::batch([save_config(app), probe_disk_cache_size(&app.pipeline)])
        }
    }
}
mod widget;
