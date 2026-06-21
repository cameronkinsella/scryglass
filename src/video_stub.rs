//! No-op stand-in for video playback when the `video` cargo feature is
//! disabled. Same API surface, but `is_video` never matches, so a session is
//! never constructed and the methods are unreachable.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub const EXTENSIONS: &[&str] = &[];

pub fn is_video(_path: &Path) -> bool {
    false
}

pub fn first_frame_thumb(_path: &Path, _max_dim: u32) -> Option<crate::media::ThumbData> {
    None
}

pub fn clean_extraction_dir() {}

pub fn extraction_dir() -> PathBuf {
    std::env::temp_dir().join("scryglass-video")
}

#[allow(dead_code)] // mirrors the real module so shared code compiles
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

#[allow(dead_code)] // mirrors the real module so shared code compiles
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvRange {
    Limited,
    Full,
}

#[allow(dead_code)] // mirrors the real module so shared code compiles
pub struct VideoFrame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub matrix: YuvMatrix,
    pub range: YuvRange,
    pub timestamp: Duration,
}

impl VideoFrame {
    #[allow(dead_code)] // mirrors the real module so shared code compiles
    pub fn to_rgba(&self) -> (u32, u32, Vec<u8>) {
        (self.width, self.height, Vec::new())
    }
}

pub struct TempFileGuard;

impl TempFileGuard {
    pub fn new(_path: PathBuf) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self)
    }
}

pub struct VideoSession {
    pub playing: bool,
    pub looping: bool,
    pub volume: f32,
    pub muted: bool,
    pub path: PathBuf,
    pub temp: Option<std::sync::Arc<TempFileGuard>>,
}

impl VideoSession {
    pub fn open(
        path: PathBuf,
        _start: Duration,
        volume: f32,
        muted: bool,
        looping: bool,
        _hardware: bool,
    ) -> Self {
        Self {
            playing: false,
            looping,
            volume,
            muted,
            path,
            temp: None,
        }
    }

    pub fn reopen_at(&self, _start: Duration) -> Self {
        Self {
            playing: false,
            looping: self.looping,
            volume: self.volume,
            muted: self.muted,
            path: self.path.clone(),
            temp: None,
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
