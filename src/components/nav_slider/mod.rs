#[derive(Debug, Clone)]
pub enum Message {
    Changed(usize),
    Released,
    DwellCheck,
}
use std::time::Duration;

use iced::Element;
use iced::Task;

use crate::app::state::SliderDrag;
use crate::app::update::{NavTarget, complete_navigation, navigate, scrub_to};
use crate::app::{App, Message as AppMessage};

/// How long the slider must rest on an off-screen image before it loads.
const DWELL: Duration = Duration::from_millis(200);

pub(crate) fn view(app: &App) -> Element<'_, AppMessage> {
    let Some(viewer) = app.viewer() else {
        return iced::widget::row![].into();
    };

    let value = viewer
        .slider_drag
        .map(|d| d.target)
        .unwrap_or_else(|| viewer.nav.cursor());

    widget::nav_slider(value, viewer.nav.len()).map(AppMessage::NavSlider)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Changed(index) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let index = index.min(viewer.nav.len().saturating_sub(1));
            let cursor = viewer.nav.cursor();
            let needs_full = !viewer.has_full(&viewer.nav.files()[index]);
            let since = match viewer.slider_drag {
                Some(d) if d.target == index => d.since,
                _ => iced::time::Instant::now(),
            };
            viewer.slider_drag = Some(SliderDrag {
                target: index,
                since,
            });
            // Arm a dwell whenever the sharp image isn't ready (a blurred or
            // cold target). One check is armed at a time and reschedules
            // itself, so a sweep never spawns one per step.
            let arm = needs_full && !viewer.dwell_pending;
            if arm {
                viewer.dwell_pending = true;
            }

            // Move to the frame immediately, showing its blur or a spinner; the
            // sharp image loads on the dwell or on release. The slider centers
            // the cursor in the filmstrip.
            let scrub = if index != cursor {
                scrub_to(app, index, true)
            } else {
                Task::none()
            };
            if arm {
                Task::batch([scrub, dwell_after(DWELL)])
            } else {
                scrub
            }
        }

        // A dwell check came due: load the rested image, or reschedule if the
        // slider has moved since the timer was armed.
        Message::DwellCheck => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.dwell_pending = false;
            let Some(drag) = viewer.slider_drag else {
                return Task::none();
            };
            let target = drag.target;
            if viewer.has_full(&viewer.nav.files()[target]) {
                return Task::none();
            }
            let elapsed = drag.since.elapsed();
            if elapsed < DWELL {
                viewer.dwell_pending = true;
                return dwell_after(DWELL - elapsed);
            }
            // Load it: in place if the scrub already moved here, else navigate.
            if viewer.nav.cursor() == target {
                complete_navigation(app, target, true)
            } else {
                navigate(app, NavTarget::Index(target))
            }
        }

        Message::Released => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            viewer.dwell_pending = false;
            let Some(drag) = viewer.slider_drag.take() else {
                return Task::none();
            };
            if drag.target == viewer.nav.cursor() {
                complete_navigation(app, drag.target, true)
            } else {
                navigate(app, NavTarget::Index(drag.target))
            }
        }
    }
}

/// A one-shot task that fires a dwell check after `delay`. Lazy so it never
/// builds a timer until iced runs it.
fn dwell_after(delay: Duration) -> Task<AppMessage> {
    Task::perform(
        async move {
            tokio::time::sleep(delay).await;
        },
        |_| AppMessage::NavSlider(Message::DwellCheck),
    )
}

mod widget;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{cache_thumb, viewing_app};

    #[test]
    fn changed_sets_a_clamped_drag_target() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(&mut app, Message::Changed(99));
        let drag = app
            .viewer()
            .unwrap()
            .slider_drag
            .expect("drag should be set");
        assert_eq!(drag.target, 2); // clamped to the last index
    }

    #[test]
    fn releasing_consumes_the_drag_and_keeps_the_cursor() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(&mut app, Message::Changed(2));
        // The scrub moves the cursor immediately, even with no thumbnail.
        assert_eq!(app.viewer().unwrap().nav.cursor(), 2);
        let _ = update(&mut app, Message::Released);
        let viewer = app.viewer().unwrap();
        assert!(viewer.slider_drag.is_none());
        assert_eq!(viewer.nav.cursor(), 2);
    }

    #[tokio::test]
    async fn changed_to_a_displayable_target_scrubs_live() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        cache_thumb(&mut app, "b.png", 8, 8);
        let _ = update(&mut app, Message::Changed(1));
        // The cursor follows the slider immediately, showing the cached blur.
        assert_eq!(app.viewer().unwrap().nav.cursor(), 1);
    }

    #[test]
    fn a_blurred_target_still_arms_the_dwell() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        // A thumbnail makes it displayable (a blur shows), but the sharp image
        // still isn't loaded, so the dwell must arm to fetch it.
        cache_thumb(&mut app, "b.png", 8, 8);
        let _ = update(&mut app, Message::Changed(1));
        assert!(app.viewer().unwrap().dwell_pending);
    }

    fn drag_at(target: usize, rested: bool) -> SliderDrag {
        let since = if rested {
            iced::time::Instant::now() - DWELL - Duration::from_millis(10)
        } else {
            iced::time::Instant::now()
        };
        SliderDrag { target, since }
    }

    #[test]
    fn a_rested_target_loads_on_the_dwell_check() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().slider_drag = Some(drag_at(2, true));
        let _ = update(&mut app, Message::DwellCheck);
        assert_eq!(app.viewer().unwrap().pending_nav, Some(2));
    }

    #[test]
    fn a_brief_rest_reschedules_instead_of_loading() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().slider_drag = Some(drag_at(2, false));
        let _ = update(&mut app, Message::DwellCheck);
        let viewer = app.viewer().unwrap();
        assert_eq!(viewer.pending_nav, None);
        assert!(viewer.dwell_pending); // re-armed for the remaining time
    }

    #[test]
    fn a_dwell_check_skips_a_target_with_nothing_left_to_load() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        app.viewer_mut().unwrap().slider_drag = Some(drag_at(2, true));
        // A recorded load error means the target is resolved; `has_full` treats
        // it as done, so the dwell leaves it alone.
        app.viewer_mut()
            .unwrap()
            .failed_loads
            .insert("c.png".into(), "x".into());
        let _ = update(&mut app, Message::DwellCheck);
        let viewer = app.viewer().unwrap();
        assert_eq!(viewer.pending_nav, None);
        assert!(!viewer.dwell_pending);
    }

    #[tokio::test]
    async fn released_on_the_current_index_completes_in_place() {
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        let _ = update(&mut app, Message::Changed(0));
        let _ = update(&mut app, Message::Released);
        assert!(app.viewer().unwrap().slider_drag.is_none());
    }

    #[test]
    fn the_slider_renders_with_a_drag() {
        use iced_test::simulator;
        let mut app = viewing_app(&["a.png", "b.png"], 0);
        app.viewer_mut().unwrap().slider_drag = Some(SliderDrag {
            target: 1,
            since: iced::time::Instant::now(),
        });
        let _ = simulator(view(&app));
    }
}
