#[derive(Debug, Clone)]
pub enum Message {
    Changed(usize),
    Released,
}
use iced::Element;
use iced::Task;

use crate::app::state::SliderDrag;
use crate::app::update::{NavTarget, complete_navigation, navigate, scrub_to};
use crate::app::{App, Message as AppMessage};
use crate::components::empty;

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

pub(crate) fn scrub_bubble(app: &App) -> Element<'_, AppMessage> {
    match app.viewer() {
        Some(viewer) => match viewer.slider_drag {
            Some(drag) if drag.bubble => widget::scrub_bubble(
                viewer.nav.files(),
                drag.target,
                &viewer.thumbs,
                app.window_size,
                app.config.show_footer,
            ),
            _ => empty(),
        },
        None => empty(),
    }
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Changed(index) => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
            let index = index.min(viewer.nav.len().saturating_sub(1));
            let scrubbable = viewer.displayable(&viewer.nav.files()[index]);
            let bubble = !scrubbable;
            viewer.slider_drag = Some(SliderDrag {
                target: index,
                bubble,
            });

            if scrubbable && index != viewer.nav.cursor() {
                scrub_to(app, index)
            } else {
                Task::none()
            }
        }

        Message::Released => {
            let Some(viewer) = app.viewer_mut() else {
                return Task::none();
            };
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
mod widget;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::viewing_app;

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
        assert!(drag.bubble); // nothing cached, so the fallback bubble shows
    }

    #[test]
    fn released_consumes_the_drag_and_defers_navigation() {
        let mut app = viewing_app(&["a.png", "b.png", "c.png"], 0);
        let _ = update(&mut app, Message::Changed(2));
        let _ = update(&mut app, Message::Released);
        let viewer = app.viewer().unwrap();
        assert!(viewer.slider_drag.is_none());
        assert_eq!(viewer.pending_nav, Some(2));
    }
}
