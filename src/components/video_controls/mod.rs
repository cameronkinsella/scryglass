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
}
use iced::{Element, Task};

use crate::app::state::Viewer;
use crate::app::update::video_flow;
use crate::app::{App, Message as AppMessage};
use crate::video::VideoSession;

pub(crate) fn view<'a>(session: &VideoSession, viewer: &Viewer) -> Element<'a, AppMessage> {
    widget::video_controls(widget::VideoControls {
        playing: session.playing,
        position: session.position(),
        duration: session.duration(),
        seek_drag: viewer.video_seek_drag,
        volume: session.volume,
        muted: session.muted,
        looping: session.looping,
    })
    .map(AppMessage::VideoControls)
}

pub(crate) fn update(app: &mut App, message: Message) -> Task<AppMessage> {
    match message {
        Message::Tick => video_flow::tick(app),
        Message::Extracted { entry, result } => video_flow::extracted(app, entry, result),
        Message::PlayPause => video_flow::play_pause(app),
        Message::SeekDrag(secs) => video_flow::seek_drag(app, secs),
        Message::SeekRelease => video_flow::seek_release(app),
        Message::SeekBy(delta) => video_flow::seek_by(app, delta),
        Message::SetVolume(volume) => video_flow::set_volume(app, volume),
        Message::NudgeVolume(delta) => video_flow::nudge_volume(app, delta),
        Message::ToggleMute => video_flow::toggle_mute(app),
        Message::ToggleLoop => video_flow::toggle_loop(app),
    }
}
mod widget;
