//! Persistent thumbnail cache on local disk (cargo feature `disk-thumbs`).
//!
//! Layout: `cache_dir()/scryglass/thumbs/<bucket>/<entry>.sgt`, where the
//! bucket hashes the containing folder (or archive) and the entry hashes
//! the file name. Every operation here touches ONLY the local cache
//! directory. Source files are never stat'd, so offline storage can never
//! cause hangs or false purges.
//!
//! Privacy hygiene:
//! - `reconcile` deletes entries whose source vanished, fed by the folder
//!   listing the viewer already fetched, with no extra source I/O.
//! - Entries unused for [`TTL_DAYS`] are deleted at startup.
//! - The cache is LRU-trimmed to [`CAP_BYTES`] at startup.
//! - In-app delete/rename will purge entries when file ops land.
//!
//! Entry format (little endian): magic "SGT1" (version byte in the tag),
//! original width/height (u32 each), source mtime seconds (u64), source
//! size (u64), then a QOI-encoded thumbnail. Source mtime/size are stored
//! for future validation. A corrupt or unknown entry simply reads as a
//! miss. Writes go through a temp file + rename so concurrent instances
//! never observe torn entries.

use std::ffi::OsStr;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::media::ThumbData;

const MAGIC: &[u8; 4] = b"SGT1";

/// Delete entries unused for this long (checked at startup, local-only).
const TTL_DAYS: u64 = 90;

/// LRU-trim the cache to this size at startup.
const CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Re-touching an entry's last-used time is throttled to once per day to
/// avoid write churn on hot folders.
const TOUCH_GRANULARITY: Duration = Duration::from_secs(24 * 60 * 60);

/// Handle to the on-disk thumbnail store. Cheap to clone.
#[derive(Debug, Clone)]
pub struct DiskThumbs {
    root: PathBuf,
}

impl DiskThumbs {
    /// Open (creating if needed) the cache root, or `None` when disabled
    /// by config or no cache directory exists.
    pub fn create(enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        let root = dirs::cache_dir()?.join("scryglass").join("thumbs");
        fs::create_dir_all(&root).ok()?;
        Some(Self { root })
    }

    /// Cache root for tests.
    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        fs::create_dir_all(&root).ok();
        Self { root }
    }

    fn bucket_dir(&self, container: &Path) -> PathBuf {
        self.root
            .join(format!("{:016x}", fnv1a(container.as_os_str())))
    }

    fn entry_path(&self, container: &Path, name: &OsStr) -> PathBuf {
        self.bucket_dir(container)
            .join(format!("{:016x}.sgt", fnv1a(name)))
    }

    /// Load a cached thumbnail. Blocking, run on a worker.
    pub fn load(&self, container: &Path, name: &OsStr) -> Option<ThumbData> {
        let path = self.entry_path(container, name);
        let bytes = fs::read(&path).ok()?;
        let data = decode_entry(&bytes)?;

        // Refresh last-used (drives the TTL/LRU), throttled.
        if let Ok(meta) = fs::metadata(&path)
            && meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age > TOUCH_GRANULARITY)
            && let Ok(file) = fs::File::options().write(true).open(&path)
        {
            let _ = file.set_modified(SystemTime::now());
        }

        Some(data)
    }

    /// Store a thumbnail. Blocking, run on a worker.
    pub fn store(
        &self,
        container: &Path,
        name: &OsStr,
        thumb: &ThumbData,
        src_mtime: Option<SystemTime>,
        src_size: u64,
    ) {
        let Some(encoded) = encode_entry(thumb, src_mtime, src_size) else {
            return;
        };
        let path = self.entry_path(container, name);
        let Some(bucket) = path.parent() else {
            return;
        };
        if fs::create_dir_all(bucket).is_err() {
            return;
        }
        // Atomic-ish publish: never expose a torn entry.
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, &encoded).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// Delete cached entries for `container` whose source file is no
    /// longer in `live_names`. Called with the listing the viewer already
    /// fetched when opening the folder, so no source I/O happens here.
    pub fn reconcile(&self, container: &Path, live_names: &[std::ffi::OsString]) {
        let bucket = self.bucket_dir(container);
        let Ok(entries) = fs::read_dir(&bucket) else {
            return;
        };
        let live: std::collections::HashSet<String> = live_names
            .iter()
            .map(|n| format!("{:016x}.sgt", fnv1a(n)))
            .collect();

        for entry in entries.flatten() {
            let keep = entry.file_name().to_str().is_some_and(|n| live.contains(n));
            if !keep {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Startup housekeeping, local metadata only: drop entries unused for
    /// [`TTL_DAYS`], then LRU-trim to [`CAP_BYTES`]. Blocking, run on a
    /// worker.
    pub fn housekeep(&self) {
        let now = SystemTime::now();
        let ttl = Duration::from_secs(TTL_DAYS * 24 * 60 * 60);

        // (last_used, size, path) for every entry.
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let Ok(buckets) = fs::read_dir(&self.root) else {
            return;
        };
        for bucket in buckets.flatten() {
            let Ok(files) = fs::read_dir(bucket.path()) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(meta) = file.metadata() else {
                    continue;
                };
                let used = meta.modified().unwrap_or(now);
                if now.duration_since(used).is_ok_and(|age| age > ttl) {
                    let _ = fs::remove_file(file.path());
                } else {
                    entries.push((used, meta.len(), file.path()));
                }
            }
            // Drop buckets emptied by expiry.
            let _ = fs::remove_dir(bucket.path());
        }

        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        if total <= CAP_BYTES {
            return;
        }
        entries.sort_by_key(|(used, _, _)| *used);
        for (_, size, path) in entries {
            if total <= CAP_BYTES {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= size;
            }
        }
    }

    /// Remove a single entry. The source file was deleted or renamed
    /// in-app, so its thumbnail must not outlive it.
    pub fn remove(&self, container: &Path, name: &OsStr) {
        let _ = fs::remove_file(self.entry_path(container, name));
    }

    /// Remove the entire cache (settings "Clear" button).
    pub fn clear(&self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::create_dir_all(&self.root);
    }

    /// Total bytes used on disk (settings display). Blocking, run on a
    /// worker. Local cache directory only.
    pub fn total_size(&self) -> u64 {
        let Ok(buckets) = fs::read_dir(&self.root) else {
            return 0;
        };
        buckets
            .flatten()
            .filter_map(|bucket| fs::read_dir(bucket.path()).ok())
            .flat_map(|files| files.flatten())
            .filter_map(|file| file.metadata().ok())
            .map(|meta| meta.len())
            .sum()
    }
}

fn encode_entry(
    thumb: &ThumbData,
    src_mtime: Option<SystemTime>,
    src_size: u64,
) -> Option<Vec<u8>> {
    let image = image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.pixels.clone())?;
    let mut qoi = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut qoi, image::ImageFormat::Qoi)
        .ok()?;
    let qoi = qoi.into_inner();

    let mtime_secs = src_mtime
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut out = Vec::with_capacity(32 + qoi.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&thumb.original_size.0.to_le_bytes());
    out.extend_from_slice(&thumb.original_size.1.to_le_bytes());
    out.extend_from_slice(&mtime_secs.to_le_bytes());
    out.extend_from_slice(&src_size.to_le_bytes());
    out.extend_from_slice(&qoi);
    Some(out)
}

fn decode_entry(bytes: &[u8]) -> Option<ThumbData> {
    if bytes.len() < 28 || &bytes[..4] != MAGIC {
        return None;
    }
    let orig_w = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let orig_h = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    // bytes[12..28]: source mtime + size, reserved for future validation.

    let image = image::ImageReader::with_format(Cursor::new(&bytes[28..]), image::ImageFormat::Qoi)
        .decode()
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Some(ThumbData {
        width,
        height,
        pixels: image.into_raw(),
        original_size: (orig_w, orig_h),
    })
}

/// FNV-1a 64, a stable hash for persistent cache keys (std's hasher is
/// not stable across releases).
fn fnv1a(s: &OsStr) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn thumb(w: u32, h: u32) -> ThumbData {
        ThumbData {
            width: w,
            height: h,
            pixels: vec![128; (w * h * 4) as usize],
            original_size: (w * 10, h * 10),
        }
    }

    #[test]
    fn store_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cache = DiskThumbs::at(dir.path().to_path_buf());
        let folder = Path::new(r"C:\photos");

        cache.store(folder, OsStr::new("a.jpg"), &thumb(8, 4), None, 1234);
        let loaded = cache.load(folder, OsStr::new("a.jpg")).expect("hit");
        assert_eq!((loaded.width, loaded.height), (8, 4));
        assert_eq!(loaded.original_size, (80, 40));
        assert_eq!(loaded.pixels, thumb(8, 4).pixels);

        assert!(cache.load(folder, OsStr::new("missing.jpg")).is_none());
        assert!(
            cache
                .load(Path::new(r"D:\other"), OsStr::new("a.jpg"))
                .is_none()
        );
    }

    #[test]
    fn reconcile_purges_deleted_sources_only() {
        let dir = TempDir::new().unwrap();
        let cache = DiskThumbs::at(dir.path().to_path_buf());
        let folder = Path::new(r"C:\photos");

        cache.store(folder, OsStr::new("keep.jpg"), &thumb(4, 4), None, 1);
        cache.store(folder, OsStr::new("ghost.jpg"), &thumb(4, 4), None, 1);

        cache.reconcile(folder, &[OsStr::new("keep.jpg").to_owned()]);

        assert!(cache.load(folder, OsStr::new("keep.jpg")).is_some());
        assert!(cache.load(folder, OsStr::new("ghost.jpg")).is_none());
    }

    #[test]
    fn housekeep_expires_old_entries() {
        let dir = TempDir::new().unwrap();
        let cache = DiskThumbs::at(dir.path().to_path_buf());
        let folder = Path::new(r"C:\photos");

        cache.store(folder, OsStr::new("old.jpg"), &thumb(4, 4), None, 1);
        cache.store(folder, OsStr::new("new.jpg"), &thumb(4, 4), None, 1);

        // Age one entry past the TTL.
        let old_path = cache.entry_path(folder, OsStr::new("old.jpg"));
        let file = fs::File::options().write(true).open(&old_path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs((TTL_DAYS + 1) * 24 * 60 * 60))
            .unwrap();
        drop(file);

        cache.housekeep();

        assert!(cache.load(folder, OsStr::new("old.jpg")).is_none());
        assert!(cache.load(folder, OsStr::new("new.jpg")).is_some());
    }

    #[test]
    fn corrupt_entries_read_as_misses() {
        let dir = TempDir::new().unwrap();
        let cache = DiskThumbs::at(dir.path().to_path_buf());
        let folder = Path::new(r"C:\photos");

        let path = cache.entry_path(folder, OsStr::new("bad.jpg"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not a thumbnail at all").unwrap();

        assert!(cache.load(folder, OsStr::new("bad.jpg")).is_none());
    }

    #[test]
    fn clear_empties_the_cache() {
        let dir = TempDir::new().unwrap();
        let cache = DiskThumbs::at(dir.path().to_path_buf());
        let folder = Path::new(r"C:\photos");

        cache.store(folder, OsStr::new("a.jpg"), &thumb(4, 4), None, 1);
        cache.clear();
        assert!(cache.load(folder, OsStr::new("a.jpg")).is_none());
    }
}
