//! No-op stand-in for video playback when the `video` cargo feature is
//! disabled. Same API surface, but `is_video` never matches, so a session is
//! never constructed and the methods are unreachable.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EXTENSIONS: &[&str] = &[];

pub fn is_video(_path: &Path) -> bool {
    false
}

pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub timestamp: Duration,
}

pub struct VideoSession {
    pub playing: bool,
    pub looping: bool,
    pub volume: f32,
    pub muted: bool,
    pub path: PathBuf,
}

impl VideoSession {
    pub fn open(path: PathBuf, _start: Duration, volume: f32, muted: bool, looping: bool) -> Self {
        Self {
            playing: false,
            looping,
            volume,
            muted,
            path,
        }
    }

    pub fn position(&self) -> Duration {
        Duration::ZERO
    }

    pub fn duration(&self) -> Option<Duration> {
        None
    }

    pub fn poll(&mut self) -> Option<VideoFrame> {
        None
    }

    pub fn finished(&self) -> bool {
        true
    }

    pub fn pause(&mut self) {}

    pub fn play(&mut self) {}

    pub fn set_volume(&mut self, _volume: f32) {}

    pub fn toggle_mute(&mut self) {}
}
