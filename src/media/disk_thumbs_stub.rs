//! No-op stand-in for the disk thumbnail cache when the `disk-thumbs`
//! cargo feature is disabled. Same API, does nothing, so call sites stay
//! free of cfg noise.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::SystemTime;

use crate::media::ThumbData;

#[derive(Debug, Clone)]
pub struct DiskThumbs;

impl DiskThumbs {
    pub fn create(_enabled: bool) -> Option<Self> {
        None
    }

    pub fn load(&self, _container: &Path, _name: &OsStr) -> Option<ThumbData> {
        None
    }

    pub fn store(
        &self,
        _container: &Path,
        _name: &OsStr,
        _thumb: &ThumbData,
        _src_mtime: Option<SystemTime>,
        _src_size: u64,
    ) {
    }

    pub fn reconcile(&self, _container: &Path, _live_names: &[OsString]) {}

    pub fn remove(&self, _container: &Path, _name: &OsStr) {}

    pub fn housekeep(&self) {}

    pub fn clear(&self) {}

    pub fn total_size(&self) -> u64 {
        0
    }
}

pub fn store_size_on_disk() -> u64 {
    0
}
