//! Measuring the real image-area size from iced's layout instead of estimating
//! it as window-minus-chrome. The estimate is a few pixels off (uncounted
//! spacing between chrome elements), knocking the fit zoom and the view-res
//! bake out of step with what is on screen and softening the demote. A widget
//! operation reads the image area's laid-out bounds by id, the true viewport.

use iced::advanced::widget::operation::Outcome;
use iced::advanced::widget::{Id, Operation, operate};
use iced::window;
use iced::{Rectangle, Task};

use crate::app::Window;

use crate::app::Message;
use crate::app::update::window::Message as WindowMessage;

/// The widget id of a window's image-area container, derived from the window id so
/// it is stable across frames (the measuring operation matches on it) and unique
/// per window (the operation walks every window's tree).
pub(crate) fn image_area_id(window: window::Id) -> Id {
    Id::from(format!("scryglass-image-area-{window:?}"))
}

/// Whether a message puts a (possibly first) image on screen, so the image area
/// should be remeasured. The first such message after an open corrects the viewport
/// from the chrome estimate. Later ones are cheap once it already agrees.
pub(crate) fn displays_image(message: &Message) -> bool {
    use crate::app::update::media;
    matches!(
        message,
        Message::Media(
            media::Message::TextureReady { .. }
                | media::Message::Decoded { .. }
                | media::Message::AnimDecoded { .. }
                | media::Message::ViewRotated { .. }
                | media::Message::PromoteCurrent(_)
        )
    )
}

/// Read the laid-out bounds of `window`'s image area, delivered back as an
/// `ImageAreaMeasured` tagged with the current window size (so a measurement the
/// window has resized past can be dropped). The position rides along with the
/// size: the blur snap needs the area's offset in the window. Yields nothing
/// until the container has been laid out.
pub(crate) fn image_area(window: &Window) -> Task<Message> {
    let at = window.window_size;
    operate(MeasureBounds {
        target: image_area_id(window.id),
        found: None,
    })
    .map(move |area| Message::Window(WindowMessage::ImageAreaMeasured { area, at }))
}

/// Captures the bounds of the container whose id matches `target`.
struct MeasureBounds {
    target: Id,
    found: Option<Rectangle>,
}

impl Operation<Rectangle> for MeasureBounds {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Rectangle>)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.target) {
            self.found = Some(bounds);
        }
    }

    fn finish(&self) -> Outcome<Rectangle> {
        match self.found {
            Some(bounds) => Outcome::Some(bounds),
            None => Outcome::None,
        }
    }
}
