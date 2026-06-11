//! Cancellable, prioritized image loading.
//!
//! Loads go through three steps: async file read, decode on a blocking
//! worker, GPU upload (done by the caller), with staleness checks between
//! steps. Every navigation bumps a generation counter, and loads fired for an
//! older generation bail out with [`MediaError::Cancelled`] at the next
//! checkpoint instead of wasting a worker on a result nobody wants.
//! The caller re-fires cancelled loads that are still relevant.
//!
//! Two semaphore lanes keep the image being viewed ahead of prefetch:
//! a stampede of prefetch requests can never starve the current image.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use super::archive::ArchiveIndex;
use super::disk_thumbs::DiskThumbs;
use super::registry::{self, DecodeOpts};
use super::{DecodedMedia, MediaError, ThumbData, thumbs};

/// Which queue a load belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The image the user is looking at right now.
    Current,
    /// A neighbor being warmed up in the background.
    Prefetch,
}

/// Where a viewer session's bytes come from: the filesystem, or entries
/// inside an opened archive. Navigation paths are filesystem paths in the
/// first case and archive entry names in the second.
#[derive(Debug, Clone)]
pub enum Source {
    Fs,
    Archive(Arc<ArchiveIndex>),
}

impl Source {
    /// Read a whole file/entry.
    async fn read_all(&self, path: &PathBuf) -> Result<Vec<u8>, MediaError> {
        match self {
            Source::Fs => tokio::fs::read(path)
                .await
                .map_err(|e| MediaError::Read(e.to_string())),
            Source::Archive(index) => {
                let index = index.clone();
                let entry = path.clone();
                tokio::task::spawn_blocking(move || index.read(&entry))
                    .await
                    .map_err(|e| MediaError::Read(e.to_string()))?
                    .map_err(|e| MediaError::Read(e.to_string()))
            }
        }
    }

    /// Read up to `n` bytes from the start of a file/entry.
    async fn read_start(&self, path: &PathBuf, n: usize) -> Result<Vec<u8>, MediaError> {
        match self {
            Source::Fs => read_prefix(path, n)
                .await
                .map_err(|e| MediaError::Read(e.to_string())),
            // Archive entries decompress as a stream anyway, so read fully
            // and truncate.
            Source::Archive(_) => {
                let mut bytes = self.read_all(path).await?;
                bytes.truncate(n);
                Ok(bytes)
            }
        }
    }
}

/// Shared load orchestrator. Cheap to clone.
#[derive(Clone)]
pub struct Pipeline {
    generation: Arc<AtomicU64>,
    current_lane: Arc<Semaphore>,
    prefetch_lane: Arc<Semaphore>,
    thumb_lane: Arc<Semaphore>,
    urgent_thumb_lane: Arc<Semaphore>,
    /// Persistent thumbnail store, `None` when disabled by build or config.
    disk: Option<DiskThumbs>,
}

impl Pipeline {
    pub fn new(disk: Option<DiskThumbs>) -> Self {
        let prefetch_permits = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(2))
            .unwrap_or(2);
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            current_lane: Arc::new(Semaphore::new(2)),
            prefetch_lane: Arc::new(Semaphore::new(prefetch_permits)),
            thumb_lane: Arc::new(Semaphore::new(2)),
            // Wide enough that scrubbing never waits on the background
            // queue, bounded so a long key-hold can't flood the I/O pool.
            urgent_thumb_lane: Arc::new(Semaphore::new(8)),
            disk,
        }
    }

    /// The persistent thumbnail store, if enabled.
    pub fn disk(&self) -> Option<&DiskThumbs> {
        self.disk.as_ref()
    }

    /// The generation of the most recent navigation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Mark a new navigation. Loads fired for older generations become stale.
    pub fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Load and decode `path`. Returns [`MediaError::Cancelled`] if a newer
    /// generation supersedes this load before it finishes decoding.
    pub fn load(
        &self,
        source: Source,
        path: PathBuf,
        opts: DecodeOpts,
        lane: Lane,
        generation: u64,
    ) -> impl Future<Output = Result<DecodedMedia, MediaError>> + Send + 'static {
        let live = self.generation.clone();
        let semaphore = match lane {
            Lane::Current => self.current_lane.clone(),
            Lane::Prefetch => self.prefetch_lane.clone(),
        };

        let disk = self.disk.clone();

        async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| MediaError::Cancelled)?;
            if live.load(Ordering::SeqCst) != generation {
                return Err(MediaError::Cancelled);
            }

            // Async read: a stalled read parks here without occupying
            // a decode worker or blocking thread.
            let bytes = source.read_all(&path).await?;
            if live.load(Ordering::SeqCst) != generation {
                return Err(MediaError::Cancelled);
            }

            let cache_key = cache_key(&source, &path);
            let media = tokio::task::spawn_blocking(move || {
                let src_size = bytes.len() as u64;
                let magic = &bytes[..bytes.len().min(16)];
                let format = registry::global()
                    .find(&path, magic)
                    .ok_or(MediaError::Unsupported)?;
                let media = format.decode(&bytes, &opts)?;

                // Decodes produce a thumbnail nearly for free. Persist it
                // so the next session opens this folder warm.
                if let (Some(disk), DecodedMedia::Static(img)) = (&disk, &media)
                    && let Some(thumb) = &img.thumbnail
                {
                    let (container, name) = &cache_key;
                    disk.store(container, name, thumb, None, src_size);
                }
                Ok::<_, MediaError>(media)
            })
            .await
            .map_err(|e| MediaError::Decode(e.to_string()))??;

            if live.load(Ordering::SeqCst) != generation {
                return Err(MediaError::Cancelled);
            }
            Ok(media)
        }
    }
}

/// The (container, entry-name) pair identifying a file in the disk
/// thumbnail cache: its folder + file name on disk, or the archive path +
/// full entry name inside an archive.
pub fn cache_key(source: &Source, path: &std::path::Path) -> (PathBuf, std::ffi::OsString) {
    match source {
        Source::Fs => (
            path.parent()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf(),
            path.file_name().unwrap_or_default().to_owned(),
        ),
        Source::Archive(index) => (index.archive_path.clone(), path.as_os_str().to_owned()),
    }
}

/// How soon a thumbnail is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbUrgency {
    /// Placeholder for the image on screen right now: skips the queue and
    /// only tries the cheap EXIF prefix probe (the full decode is already
    /// racing on the current lane).
    Urgent,
    /// Filmstrip/background work: queued on the thumb lane, and falls back
    /// to a full decode when there's no embedded preview.
    Background,
}

impl Pipeline {
    /// Produce a thumbnail for `path`. Thumbnails are never cancelled by
    /// navigation. Every result is useful to the filmstrip eventually.
    pub fn load_thumb(
        &self,
        source: Source,
        path: PathBuf,
        urgency: ThumbUrgency,
    ) -> impl Future<Output = Result<ThumbData, MediaError>> + Send + 'static {
        let semaphore = match urgency {
            ThumbUrgency::Urgent => self.urgent_thumb_lane.clone(),
            ThumbUrgency::Background => self.thumb_lane.clone(),
        };
        let disk = self.disk.clone();

        async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|_| MediaError::Cancelled)?;

            let (container, name) = cache_key(&source, &path);

            // Fastest path: a thumbnail persisted by an earlier session.
            if let Some(disk) = disk.clone() {
                let (c, n) = (container.clone(), name.clone());
                let hit = tokio::task::spawn_blocking(move || disk.load(&c, &n))
                    .await
                    .ok()
                    .flatten();
                if let Some(thumb) = hit {
                    return Ok(thumb);
                }
            }

            // Cheap path: embedded EXIF preview from the file prefix.
            let prefix = source.read_start(&path, thumbs::PREFIX_LEN).await?;
            let from_prefix =
                tokio::task::spawn_blocking(move || thumbs::thumb_from_prefix(&prefix))
                    .await
                    .map_err(|e| MediaError::Decode(e.to_string()))?;
            if let Some(thumb) = from_prefix {
                persist(&disk, &container, &name, &thumb).await;
                return Ok(thumb);
            }
            if urgency == ThumbUrgency::Urgent {
                return Err(MediaError::Unsupported);
            }

            // Background fallback: decode the whole file and downscale.
            let bytes = source.read_all(&path).await?;
            let thumb = tokio::task::spawn_blocking(move || {
                thumbs::thumb_from_bytes(&bytes).ok_or(MediaError::Unsupported)
            })
            .await
            .map_err(|e| MediaError::Decode(e.to_string()))??;
            persist(&disk, &container, &name, &thumb).await;
            Ok(thumb)
        }
    }
}

/// Write a thumbnail to the persistent store, off-thread.
async fn persist(
    disk: &Option<DiskThumbs>,
    container: &std::path::Path,
    name: &std::ffi::OsStr,
    thumb: &ThumbData,
) {
    let Some(disk) = disk.clone() else {
        return;
    };
    let container = container.to_path_buf();
    let name = name.to_owned();
    let thumb = ThumbData {
        width: thumb.width,
        height: thumb.height,
        pixels: thumb.pixels.clone(),
        original_size: thumb.original_size,
    };
    let _ = tokio::task::spawn_blocking(move || {
        disk.store(&container, &name, &thumb, None, 0);
    })
    .await;
}

/// Read up to `n` bytes from the start of a file.
async fn read_prefix(path: &std::path::Path, n: usize) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    loop {
        let read = file.read(&mut buf[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
        if filled == n {
            break;
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn write_png(dir: &TempDir, name: &str, w: u32, h: u32) -> PathBuf {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, out.into_inner()).unwrap();
        path
    }

    #[tokio::test]
    async fn load_decodes_a_png_from_disk() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new(None);
        let generation = pipeline.generation();

        let media = pipeline
            .load(
                Source::Fs,
                path,
                DecodeOpts::default(),
                Lane::Current,
                generation,
            )
            .await
            .unwrap();
        let DecodedMedia::Static(img) = media;
        assert_eq!((img.width, img.height), (4, 2));
    }

    #[tokio::test]
    async fn stale_generation_is_cancelled_before_read() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new(None);
        let generation = pipeline.generation();

        // Supersede the load before it runs.
        pipeline.bump_generation();

        let result = pipeline
            .load(
                Source::Fs,
                path,
                DecodeOpts::default(),
                Lane::Current,
                generation,
            )
            .await;
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }

    #[tokio::test]
    async fn queued_load_cancels_when_superseded_while_waiting() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new(None);
        let generation = pipeline.generation();

        // Hold every permit so the load parks at the semaphore.
        let held: Vec<_> = (0..2)
            .map(|_| pipeline.current_lane.clone().try_acquire_owned().unwrap())
            .collect();

        let task = tokio::spawn(pipeline.load(
            Source::Fs,
            path,
            DecodeOpts::default(),
            Lane::Current,
            generation,
        ));

        pipeline.bump_generation();
        drop(held);

        let result = task.await.unwrap();
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }

    #[tokio::test]
    async fn urgent_thumb_fails_fast_without_embedded_preview() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new(None);
        let result = pipeline
            .load_thumb(Source::Fs, path, ThumbUrgency::Urgent)
            .await;
        assert!(matches!(result, Err(MediaError::Unsupported)));
    }

    #[tokio::test]
    async fn background_thumb_falls_back_to_full_decode() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 600, 300);
        let pipeline = Pipeline::new(None);
        let thumb = pipeline
            .load_thumb(Source::Fs, path, ThumbUrgency::Background)
            .await
            .expect("fallback should produce a thumbnail");
        assert_eq!(thumb.original_size, (600, 300));
    }

    #[tokio::test]
    async fn missing_file_is_a_read_error() {
        let pipeline = Pipeline::new(None);
        let generation = pipeline.generation();
        let result = pipeline
            .load(
                Source::Fs,
                PathBuf::from("definitely/not/here.png"),
                DecodeOpts::default(),
                Lane::Current,
                generation,
            )
            .await;
        assert!(matches!(result, Err(MediaError::Read(_))));
    }

    #[tokio::test]
    async fn unsupported_bytes_are_an_unsupported_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("notes.xyz");
        std::fs::write(&path, b"this is not an image at all").unwrap();
        let pipeline = Pipeline::new(None);
        let generation = pipeline.generation();
        let result = pipeline
            .load(
                Source::Fs,
                path,
                DecodeOpts::default(),
                Lane::Current,
                generation,
            )
            .await;
        assert!(matches!(result, Err(MediaError::Unsupported)));
    }
}
