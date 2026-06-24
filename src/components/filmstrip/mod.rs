#[derive(Debug, Clone)]
pub enum Message {
    Clicked(usize),
    Scroll(f32),
    Scrolled(f32),
    SettleCheck,
}
use std::time::Duration;

use iced::Element;
use iced::Task;

use crate::app::update::{complete_navigation, fire_thumbnailer};
use crate::app::{App, Message as AppMessage};

/// How long the filmstrip must stop scrolling before the visible row claims
/// the thumbnail lane.
const SETTLE: Duration = Duration::from_millis(100);

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    let Some(viewer) = app.viewer() else {
        return iced::widget::row![].into();
    };

    widget::filmstrip(
        viewer.nav.files(),
        viewer.nav.cursor(),
        &viewer.thumbs,
        viewer.filmstrip_scroll_x,
        app.window_size.width,
    )
    .map(AppMessage::Filmstrip)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Scroll(delta_y) => {
            let offset = iced::widget::scrollable::AbsoluteOffset {
                x: -delta_y * 60.0,
                y: 0.0,
            };
            iced::widget::operation::scroll_by(widget::filmstrip_id(), offset)
        }

        Message::Scrolled(x) => {
            let window_w = app.window_size.width;
            let show = app.config.show_filmstrip;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.filmstrip_scroll_x = x;
            if widget::cursor_on_screen(x, viewer.nav.cursor(), window_w) {
                // Restart the cursor fan only if it drained, so repeated scroll
                // events don't pile on extra chains.
                if viewer.in_flight_thumbs.is_empty() {
                    Task::batch(fire_thumbnailer(&pipeline, viewer, 3, window_w, show))
                } else {
                    Task::none()
                }
            } else {
                viewer.filmstrip_scrolled_at = iced::time::Instant::now();
                // Arm one settle check at a time; it reschedules itself.
                if viewer.visible_settle_pending {
                    Task::none()
                } else {
                    viewer.visible_settle_pending = true;
                    settle_after(SETTLE)
                }
            }
        }

        Message::SettleCheck => {
            let window_w = app.window_size.width;
            let show = app.config.show_filmstrip;
            let pipeline = app.pipeline.clone();
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.visible_settle_pending = false;
            let elapsed = viewer.filmstrip_scrolled_at.elapsed();
            if elapsed < SETTLE {
                viewer.visible_settle_pending = true;
                return settle_after(SETTLE - elapsed);
            }
            if widget::cursor_on_screen(viewer.filmstrip_scroll_x, viewer.nav.cursor(), window_w) {
                return Task::none();
            }
            pipeline.bump_thumb_generation();
            viewer.in_flight_thumbs.clear();
            Task::batch(fire_thumbnailer(&pipeline, viewer, 3, window_w, show))
        }

        Message::Clicked(index) => complete_navigation(app, index, true),
    }
}

/// A one-shot task that fires a settle check after `delay`. Lazy so it never
/// builds a timer until iced runs it.
fn settle_after(delay: Duration) -> Task<AppMessage> {
    Task::perform(
        async move {
            tokio::time::sleep(delay).await;
        },
        |_| AppMessage::Filmstrip(Message::SettleCheck),
    )
}

pub(crate) use widget::{
    center_offset, cursor_on_screen, filmstrip_id, keep_visible_offset, open_offset, visible_range,
};
mod widget;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

    #[test]
    fn scrolled_records_the_offset() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::Scrolled(120.0));
        assert_eq!(app.viewer().unwrap().filmstrip_scroll_x, 120.0);
    }

    #[test]
    fn clicking_opens_the_target_instantly() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(&mut app, Message::Clicked(2));
        // The cursor jumps straight to the clicked frame (no deferred wait).
        assert_eq!(app.viewer().unwrap().nav.cursor(), 2);
    }

    fn big_app() -> crate::app::App {
        let ns: Vec<String> = (0..50).map(|i| format!("{i:04}.png")).collect();
        let refs: Vec<&str> = ns.iter().map(String::as_str).collect();
        viewing_app(&refs, 0)
    }

    #[test]
    fn scrolling_the_cursor_off_screen_arms_the_settle() {
        let mut app = big_app();
        let _ = update(&mut app, Message::Scrolled(3000.0));
        assert!(app.viewer().unwrap().visible_settle_pending);
    }

    #[test]
    fn scrolling_with_the_cursor_on_screen_does_not_arm_the_settle() {
        let mut app = big_app();
        // A nudge that leaves cursor 0 still in view.
        let _ = update(&mut app, Message::Scrolled(10.0));
        assert!(!app.viewer().unwrap().visible_settle_pending);
    }

    #[test]
    fn a_settled_off_screen_scroll_hands_the_lane_to_the_visible_row() {
        let mut app = big_app();
        {
            let v = app.viewer_mut().unwrap();
            v.in_flight_thumbs.insert("stale.png".into());
            v.filmstrip_scroll_x = 3000.0;
            v.filmstrip_scrolled_at = iced::time::Instant::now() - SETTLE * 2;
            v.visible_settle_pending = true;
        }
        let gen_before = app.pipeline.thumb_generation();
        let _ = update(&mut app, Message::SettleCheck);
        // The stale neighborhood is dropped and a new generation begins.
        assert_eq!(app.pipeline.thumb_generation(), gen_before + 1);
        let v = app.viewer().unwrap();
        assert!(
            !v.in_flight_thumbs
                .contains(std::path::Path::new("stale.png"))
        );
        assert!(!v.visible_settle_pending);
    }

    #[test]
    fn an_unsettled_check_reschedules_without_firing() {
        let mut app = big_app();
        {
            let v = app.viewer_mut().unwrap();
            v.filmstrip_scroll_x = 3000.0;
            v.filmstrip_scrolled_at = iced::time::Instant::now();
            v.visible_settle_pending = true;
        }
        let gen_before = app.pipeline.thumb_generation();
        let _ = update(&mut app, Message::SettleCheck);
        // Still inside the settle window: re-armed, nothing handed over yet.
        assert!(app.viewer().unwrap().visible_settle_pending);
        assert_eq!(app.pipeline.thumb_generation(), gen_before);
    }
}
