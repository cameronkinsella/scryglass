//! Video transport controls: an overlay along the bottom of the image
//! area, shown while paused or when the cursor is near the bottom.

use std::time::Duration;

use iced::widget::{button, container, row, slider, text};
use iced::{Alignment, Element, Length};

use crate::app::VideoMessage;
use crate::ui::{icons, theme};

/// Inputs for the control bar.
pub struct VideoControls {
    pub playing: bool,
    pub position: Duration,
    pub duration: Option<Duration>,
    /// Mid-drag seek position (seconds), shown instead of `position`.
    pub seek_drag: Option<f64>,
    pub volume: f32,
    pub muted: bool,
    pub looping: bool,
}

/// Render the transport bar, anchored to the bottom of the image area.
pub fn video_controls<'a>(state: VideoControls) -> Element<'a, VideoMessage> {
    let icon_button = |icon: iced::widget::Text<'a>, msg: VideoMessage| {
        button(icon.size(16))
            .on_press(msg)
            .padding([2, 8])
            .style(theme::menu_item)
    };

    let play_icon = if state.playing {
        icons::pause_fill()
    } else {
        icons::play_fill()
    };

    let shown_secs = match state.seek_drag {
        Some(dragged) => dragged,
        None => state.position.as_secs_f64(),
    };
    let total_secs = state
        .duration
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
        .max(shown_secs);

    let time = text(format!(
        "{} / {}",
        clock_label(shown_secs),
        clock_label(total_secs)
    ))
    .size(12);

    let seek = slider(
        0.0..=total_secs.max(0.1),
        shown_secs,
        VideoMessage::SeekDrag,
    )
    .on_release(VideoMessage::SeekRelease)
    .step(0.000_001)
    .width(Length::Fill);

    let volume_icon = if state.muted || state.volume == 0.0 {
        icons::volume_mute()
    } else {
        icons::volume_up()
    };
    let volume = slider(
        0.0..=1.0,
        if state.muted { 0.0 } else { state.volume },
        VideoMessage::SetVolume,
    )
    .step(0.05)
    .width(Length::Fixed(80.0));

    let loop_button = button(icons::arrow_repeat().size(16))
        .on_press(VideoMessage::ToggleLoop)
        .padding([2, 8])
        .style(if state.looping {
            theme::menu_tab_active
        } else {
            theme::menu_item
        });

    let bar = container(
        row![
            icon_button(play_icon, VideoMessage::PlayPause),
            time,
            seek,
            icon_button(volume_icon, VideoMessage::ToggleMute),
            volume,
            loop_button,
        ]
        .spacing(10)
        .padding([6, 12])
        .align_y(Alignment::Center),
    )
    .style(theme::panel)
    .width(Length::Fill);

    container(bar)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(Alignment::End)
        .into()
}

/// "m:ss" (or "h:mm:ss") for transport display.
fn clock_label(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_labels() {
        assert_eq!(clock_label(0.0), "0:00");
        assert_eq!(clock_label(75.4), "1:15");
        assert_eq!(clock_label(3671.0), "1:01:11");
    }
}
