use iced::Task;

use crate::app::{App, Message, SettingsMessage};
use crate::media::pipeline::Pipeline;

/// Persist the current config in the background. Saving is fire-and-forget:
/// the viewer must never wait on it.
pub(crate) fn save_config(app: &App) -> Task<Message> {
    Task::future(app.config.clone().save()).discard()
}

/// Measure the disk thumbnail store, off-thread.
pub(crate) fn probe_disk_cache_size(pipeline: &Pipeline) -> Task<Message> {
    let disk = pipeline.disk();
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || match &disk {
                Some(d) => d.total_size(),
                None => crate::media::disk_thumbs::store_size_on_disk(),
            })
            .await
            .unwrap_or(0)
        },
        |bytes| Message::Settings(SettingsMessage::DiskCacheSize(bytes)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::empty_app;

    #[test]
    fn save_and_probe_build_background_tasks() {
        let app = empty_app();
        let _ = save_config(&app);
        let _ = probe_disk_cache_size(&app.pipeline);
    }
}
