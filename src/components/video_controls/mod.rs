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
            seek_drag: viewer.video_seek_drag,
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
mod widget;
