use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    Extracted {
        entry: PathBuf,
        result: Result<PathBuf, String>,
    },
    PlayPause,
    SeekDrag(f64),
    SeekRelease,
    SeekBy(f64),
    SetVolume(f32),
    NudgeVolume(f32),
    ToggleMute,
    ToggleLoop,
    /// Step one frame: +1 forward, -1 back. Pauses playback.
    StepFrame(i32),
}
use iced::{Element, Task};

use crate::app::state::Viewer;
use crate::app::update::video_flow;
use crate::app::{Message as AppMessage, Shared, Window};
use crate::video::VideoSession;

pub(crate) fn view<'a>(
    session: &VideoSession,
    viewer: &Viewer,
    opacity: f32,
) -> Element<'a, AppMessage> {
    widget::video_controls(
        widget::VideoControls {
            playing: session.playing,
            position: session.position(),
            duration: session.duration(),
            seek_drag: viewer.video.seek_drag,
            volume: session.volume,
            muted: session.muted,
            looping: session.looping(),
        },
        opacity,
    )
    .map(AppMessage::VideoControls)
}

pub(crate) fn update(win: &mut Window, shared: &mut Shared, message: Message) -> Task<AppMessage> {
    match message {
        Message::Tick => video_flow::tick(win, shared),
        Message::Extracted { entry, result } => video_flow::extracted(win, shared, entry, result),
        Message::PlayPause => video_flow::play_pause(win, shared),
        Message::SeekDrag(secs) => video_flow::seek_drag(win, shared, secs),
        Message::SeekRelease => video_flow::seek_release(win, shared),
        Message::SeekBy(delta) => video_flow::seek_by(win, shared, delta),
        Message::SetVolume(volume) => video_flow::set_volume(win, shared, volume),
        Message::NudgeVolume(delta) => video_flow::nudge_volume(win, shared, delta),
        Message::ToggleMute => video_flow::toggle_mute(win, shared),
        Message::ToggleLoop => video_flow::toggle_loop(win, shared),
        Message::StepFrame(dir) => video_flow::step_frame(win, shared, dir),
    }
}

/// Whether the transport controls belong on screen: a paused or mid-scrub
/// video always shows them, a playing one only while recently active.
pub(crate) fn controls_visible(playing: bool, seeking: bool, controls_alive: bool) -> bool {
    !playing || seeking || controls_alive
}

/// Hide the cursor exactly when the controls are gone over a playing video.
pub(crate) fn hide_idle_cursor(playing: bool, seeking: bool, controls_alive: bool) -> bool {
    !controls_visible(playing, seeking, controls_alive)
}

/// Step `current` toward `target` by at most `step`, for a per-frame fade.
pub(crate) fn ease_toward(current: f32, target: f32, step: f32) -> f32 {
    if current < target {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

mod widget;

#[cfg(test)]
mod tests {
    use super::*;

    // --- controls_visible ---

    #[test]
    fn controls_visible_when_paused_seeking_or_recently_active() {
        assert!(controls_visible(false, false, false)); // paused
        assert!(controls_visible(true, true, false)); // seeking
        assert!(controls_visible(true, false, true)); // recently active
    }

    #[test]
    fn controls_hidden_while_playing_and_idle() {
        assert!(!controls_visible(true, false, false));
    }

    // --- hide_idle_cursor ---

    #[test]
    fn hide_idle_cursor_hides_when_playing_idle_and_controls_gone() {
        assert!(hide_idle_cursor(true, false, false));
    }

    #[test]
    fn hide_idle_cursor_visible_while_controls_are_up() {
        assert!(!hide_idle_cursor(true, false, true));
    }

    #[test]
    fn hide_idle_cursor_visible_while_seeking() {
        assert!(!hide_idle_cursor(true, true, false));
    }

    #[test]
    fn hide_idle_cursor_visible_when_paused() {
        assert!(!hide_idle_cursor(false, false, false));
    }

    // --- ease_toward ---

    #[test]
    fn ease_toward_rises_and_clamps_at_target() {
        assert!((ease_toward(0.0, 1.0, 0.3) - 0.3).abs() < 1e-6);
        assert_eq!(ease_toward(0.9, 1.0, 0.3), 1.0);
    }

    #[test]
    fn ease_toward_falls_and_clamps_at_target() {
        assert!((ease_toward(1.0, 0.0, 0.3) - 0.7).abs() < 1e-6);
        assert_eq!(ease_toward(0.1, 0.0, 0.3), 0.0);
    }

    #[test]
    fn ease_toward_stays_at_target() {
        assert_eq!(ease_toward(1.0, 1.0, 0.3), 1.0);
        assert_eq!(ease_toward(0.0, 0.0, 0.3), 0.0);
    }
}
