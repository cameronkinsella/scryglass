use iced::Task;

use crate::app::{App, Message};
use crate::media::pipeline::Pipeline;

/// Persist the current config in the background. Saving is fire-and-forget:
/// the viewer must never wait on it.
pub(super) fn save_config(app: &App) -> Task<Message> {
    Task::future(app.config.clone().save()).discard()
}

/// Measure the disk thumbnail store, off-thread.
pub(super) fn probe_disk_cache_size(pipeline: &Pipeline) -> Task<Message> {
    let Some(disk) = pipeline.disk() else {
        return Task::done(Message::DiskCacheSize(0));
    };
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || disk.total_size())
                .await
                .unwrap_or(0)
        },
        Message::DiskCacheSize,
    )
}
