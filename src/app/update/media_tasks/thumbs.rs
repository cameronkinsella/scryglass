//! Thumbnail jobs for the filmstrip and for load placeholders.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use iced::Task;

use crate::app::state::{Thumb, Viewer};
use crate::app::{MediaMessage, Message};
use crate::media::cache::ImageCache;
use crate::media::pipeline::{Pipeline, Source, ThumbUrgency, thumb_key};
use crate::media::{MediaError, ThumbData};

/// Where background thumbnailing should aim, as a `(center, range)` pair to
/// fan outward from: the cursor across the whole directory, or the visible row
/// alone once the cursor has scrolled off the filmstrip.
pub(crate) fn thumb_focus(
    viewer: &Viewer,
    viewport_w: f32,
    filmstrip_shown: bool,
) -> (usize, std::ops::Range<usize>) {
    let len = viewer.nav.len();
    let cursor = viewer.nav.cursor();
    if !filmstrip_shown
        || crate::components::filmstrip::cursor_on_screen(
            viewer.filmstrip_scroll_x,
            cursor,
            viewport_w,
        )
    {
        (cursor, 0..len)
    } else {
        let range =
            crate::components::filmstrip::visible_range(viewer.filmstrip_scroll_x, viewport_w, len);
        let center = range.start + (range.end - range.start) / 2;
        (center, range)
    }
}

/// Fire a thumbnail job for `path` unless one is cached, in flight, or
/// known to fail.
pub(crate) fn fire_thumb(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    path: PathBuf,
    urgency: ThumbUrgency,
) -> Task<Message> {
    if thumbs.contains(&thumb_key(&viewer.source, &path))
        || viewer.in_flight_thumbs.contains(&path)
        || viewer.failed_thumbs.contains(&path)
    {
        return Task::none();
    }

    let is_video = crate::video::is_video(&path);
    // A video thumbnail is an FFmpeg first-frame grab, which needs a real file
    // on disk, so videos inside archives get none.
    if is_video && !matches!(viewer.source, Source::Fs) {
        return Task::none();
    }

    // A full decode of this image is already in flight and will derive a
    // thumbnail. Only the cheap disk and prefix lookups are worth running.
    let cheap_only = viewer.in_flight.contains(&path);
    viewer.in_flight_thumbs.insert(path.clone());
    let generation = pipeline.thumb_generation();
    let load: Pin<Box<dyn Future<Output = Result<ThumbData, MediaError>> + Send>> = if is_video {
        Box::pin(pipeline.load_video_thumb(path.clone(), urgency, generation))
    } else {
        Box::pin(pipeline.load_thumb(
            viewer.source.clone(),
            path.clone(),
            urgency,
            generation,
            cheap_only,
        ))
    };
    Task::perform(load, move |result| {
        Message::Media(MediaMessage::ThumbLoaded {
            path: path.clone(),
            urgency,
            result: result.map(Thumb::from),
        })
    })
}

/// Fire a first-frame thumbnail for an archive video from its just-extracted
/// temp `file`, keyed under the archive `entry`. The entry has no real path for
/// FFmpeg, so this reuses the file playback already wrote. `guard` keeps it
/// alive through the decode. Skips when a thumbnail is cached, in flight, or
/// known to fail. Background urgency, since the playing video covers the wait.
pub(crate) fn fire_archive_video_thumb(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    entry: PathBuf,
    file: PathBuf,
    guard: std::sync::Arc<crate::video::TempFileGuard>,
) -> Task<Message> {
    if thumbs.contains(&thumb_key(&viewer.source, &entry))
        || viewer.in_flight_thumbs.contains(&entry)
        || viewer.failed_thumbs.contains(&entry)
    {
        return Task::none();
    }

    viewer.in_flight_thumbs.insert(entry.clone());
    let load = pipeline.load_video_thumb_from_file(file, viewer.source.clone(), entry.clone());
    Task::perform(
        async move {
            // Hold the temp file open until the first-frame decode finishes,
            // even if playback navigated away and dropped its own guard.
            let _guard = guard;
            load.await
        },
        move |result| {
            Message::Media(MediaMessage::ThumbLoaded {
                path: entry.clone(),
                urgency: ThumbUrgency::Background,
                result: result.map(Thumb::from),
            })
        },
    )
}

/// Start (or continue) background thumbnailing: up to `chains` jobs from the
/// current [`thumb_focus`].
pub(crate) fn fire_thumbnailer(
    pipeline: &Pipeline,
    thumbs: &ImageCache<Thumb>,
    viewer: &mut Viewer,
    chains: usize,
    viewport_w: f32,
    filmstrip_shown: bool,
) -> Vec<Task<Message>> {
    let mut tasks = Vec::new();
    for _ in 0..chains {
        let (center, range) = thumb_focus(viewer, viewport_w, filmstrip_shown);
        let Some(path) = viewer.next_unthumbed_in(thumbs, center, range) else {
            break;
        };
        tasks.push(fire_thumb(
            pipeline,
            thumbs,
            viewer,
            path,
            ThumbUrgency::Background,
        ));
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn archive_video_thumb_claims_the_slot_then_skips_when_present() {
        use crate::app::test_support::{cache_thumb, viewing_app};

        let mut app = viewing_app(&["clip.mp4"], 0);
        let entry = PathBuf::from("clip.mp4");
        let file = PathBuf::from("extracted.mp4");
        let guard = crate::video::TempFileGuard::new(file.clone());

        // Fresh entry: the thumb job claims the in-flight slot.
        let _ = fire_archive_video_thumb(
            &app.shared.pipeline,
            &app.shared.thumbs,
            app.window.viewer_mut().unwrap(),
            entry.clone(),
            file.clone(),
            guard.clone(),
        );
        assert!(app.viewer().unwrap().in_flight_thumbs.contains(&entry));

        // Clear the slot, cache a thumb, and confirm a second fire skips.
        app.window
            .viewer_mut()
            .unwrap()
            .in_flight_thumbs
            .remove(&entry);
        cache_thumb(&mut app, "clip.mp4", 4, 2);
        let _ = fire_archive_video_thumb(
            &app.shared.pipeline,
            &app.shared.thumbs,
            app.window.viewer_mut().unwrap(),
            entry.clone(),
            file,
            guard,
        );
        assert!(!app.viewer().unwrap().in_flight_thumbs.contains(&entry));
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:04}.png")).collect()
    }

    fn at_scroll(cursor: usize, scroll_x: f32) -> crate::app::test_support::TestApp {
        let ns = names(50);
        let refs: Vec<&str> = ns.iter().map(String::as_str).collect();
        let mut app = viewing_app(&refs, cursor);
        app.viewer_mut().unwrap().filmstrip_scroll_x = scroll_x;
        app
    }

    #[test]
    fn thumb_focus_follows_the_cursor_when_on_screen() {
        let app = at_scroll(2, 0.0);
        assert_eq!(thumb_focus(app.viewer().unwrap(), 800.0, true), (2, 0..50));
    }

    #[test]
    fn thumb_focus_switches_to_the_visible_row_off_screen() {
        let app = at_scroll(2, 3000.0);
        let (center, range) = thumb_focus(app.viewer().unwrap(), 800.0, true);
        let expected = crate::components::filmstrip::visible_range(3000.0, 800.0, 50);
        assert_eq!(range, expected);
        assert_eq!(center, expected.start + (expected.end - expected.start) / 2);
        assert_ne!(center, 2);
    }

    #[test]
    fn thumb_focus_ignores_the_scroll_when_the_filmstrip_is_hidden() {
        let app = at_scroll(2, 3000.0);
        assert_eq!(thumb_focus(app.viewer().unwrap(), 800.0, false), (2, 0..50));
    }
}
