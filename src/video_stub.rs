//! No-op stand-in for video when the `video` feature is off. `is_video`
//! never matches, so the other methods are never reached.

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YuvFormat {
    I420,
    Nv12,
}

#[allow(dead_code)] // mirrors the real module so shared code compiles
pub struct VideoFrame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub format: YuvFormat,
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
    looping: bool,
    pub volume: f32,
    pub muted: bool,
    pub path: PathBuf,
    pub temp: Option<std::sync::Arc<TempFileGuard>>,
}

/// Mirrors the real module's released-session memo so shared code compiles.
pub struct SuspendedVideo {
    pub path: PathBuf,
    pub playing: bool,
    pub looping: bool,
    pub volume: f32,
    pub muted: bool,
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

    pub fn suspend(&self, playing: bool) -> SuspendedVideo {
        SuspendedVideo {
            path: self.path.clone(),
            playing,
            looping: self.looping,
            volume: self.volume,
            muted: self.muted,
            temp: self.temp.clone(),
        }
    }

    pub fn resume(saved: &SuspendedVideo) -> Self {
        let mut session = Self::open(
            saved.path.clone(),
            Duration::ZERO,
            saved.volume,
            saved.muted,
            saved.looping,
            false,
        );
        session.playing = saved.playing;
        session.temp = saved.temp.clone();
        session
    }

    pub fn resume_at(saved: &SuspendedVideo, path: PathBuf) -> Self {
        let mut session = Self::open(
            path,
            Duration::ZERO,
            saved.volume,
            saved.muted,
            saved.looping,
            false,
        );
        session.playing = saved.playing;
        session.temp = saved.temp.clone();
        session
    }

    pub fn looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn position(&self) -> Duration {
        Duration::ZERO
    }

    pub fn duration(&self) -> Option<Duration> {
        None
    }

    pub fn frame_duration(&self) -> Option<Duration> {
        None
    }

    pub fn poll(&mut self) -> Option<VideoFrame> {
        None
    }

    pub fn failed(&self) -> Option<String> {
        None
    }

    pub fn showed_frame(&self) -> bool {
        false
    }

    pub fn finished(&self) -> bool {
        true
    }

    pub fn pause(&mut self) {}

    pub fn play(&mut self) {}

    pub fn set_volume(&mut self, _volume: f32) {}

    pub fn toggle_mute(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn session() -> VideoSession {
        let mut s = VideoSession::open(
            PathBuf::from("a.mp4"),
            Duration::ZERO,
            0.8,
            true,
            true,
            false,
        );
        s.temp = Some(TempFileGuard::new(
            std::env::temp_dir().join("scry-stub-test.mp4"),
        ));
        s.playing = true;
        s
    }

    #[test]
    fn suspend_captures_state_and_keeps_the_temp_file() {
        let s = session();
        let temp_before = s.temp.clone().unwrap();
        // The resolved playing state is what the caller passes, not the session's.
        let memo = s.suspend(false);
        assert!(!memo.playing);
        assert_eq!(memo.volume, 0.8);
        assert!(memo.muted);
        assert!(memo.looping);
        // The archive temp guard is cloned, so the file survives the session drop.
        assert!(Arc::ptr_eq(memo.temp.as_ref().unwrap(), &temp_before));
        drop(s);
        assert_eq!(Arc::strong_count(&temp_before), 2); // temp_before + the memo
    }

    #[test]
    fn resume_reconstructs_the_session() {
        let memo = session().suspend(true);
        let r = VideoSession::resume(&memo);
        assert!(r.playing);
        assert_eq!(r.volume, 0.8);
        assert!(r.muted);
        assert!(r.looping());
        assert_eq!(r.path, PathBuf::from("a.mp4"));
        assert!(r.temp.is_some());
    }
}
