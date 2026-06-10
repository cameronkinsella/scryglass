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

use tokio::sync::Semaphore;

use super::registry::{self, DecodeOpts};
use super::{DecodedMedia, MediaError};

/// Which queue a load belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The image the user is looking at right now.
    Current,
    /// A neighbor being warmed up in the background.
    Prefetch,
}

/// Shared load orchestrator. Cheap to clone.
#[derive(Clone)]
pub struct Pipeline {
    generation: Arc<AtomicU64>,
    current_lane: Arc<Semaphore>,
    prefetch_lane: Arc<Semaphore>,
}

impl Pipeline {
    pub fn new() -> Self {
        let prefetch_permits = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).max(2))
            .unwrap_or(2);
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            current_lane: Arc::new(Semaphore::new(2)),
            prefetch_lane: Arc::new(Semaphore::new(prefetch_permits)),
        }
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
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| MediaError::Read(e.to_string()))?;
            if live.load(Ordering::SeqCst) != generation {
                return Err(MediaError::Cancelled);
            }

            let media = tokio::task::spawn_blocking(move || {
                let magic = &bytes[..bytes.len().min(16)];
                let format = registry::global()
                    .find(&path, magic)
                    .ok_or(MediaError::Unsupported)?;
                format.decode(&bytes, &opts)
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

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
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
        let pipeline = Pipeline::new();
        let generation = pipeline.generation();

        let media = pipeline
            .load(path, DecodeOpts::default(), Lane::Current, generation)
            .await
            .unwrap();
        let DecodedMedia::Static(img) = media;
        assert_eq!((img.width, img.height), (4, 2));
    }

    #[tokio::test]
    async fn stale_generation_is_cancelled_before_read() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new();
        let generation = pipeline.generation();

        // Supersede the load before it runs.
        pipeline.bump_generation();

        let result = pipeline
            .load(path, DecodeOpts::default(), Lane::Current, generation)
            .await;
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }

    #[tokio::test]
    async fn queued_load_cancels_when_superseded_while_waiting() {
        let dir = TempDir::new().unwrap();
        let path = write_png(&dir, "a.png", 4, 2);
        let pipeline = Pipeline::new();
        let generation = pipeline.generation();

        // Hold every permit so the load parks at the semaphore.
        let held: Vec<_> = (0..2)
            .map(|_| pipeline.current_lane.clone().try_acquire_owned().unwrap())
            .collect();

        let task =
            tokio::spawn(pipeline.load(path, DecodeOpts::default(), Lane::Current, generation));

        pipeline.bump_generation();
        drop(held);

        let result = task.await.unwrap();
        assert!(matches!(result, Err(MediaError::Cancelled)));
    }

    #[tokio::test]
    async fn missing_file_is_a_read_error() {
        let pipeline = Pipeline::new();
        let generation = pipeline.generation();
        let result = pipeline
            .load(
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
        let pipeline = Pipeline::new();
        let generation = pipeline.generation();
        let result = pipeline
            .load(path, DecodeOpts::default(), Lane::Current, generation)
            .await;
        assert!(matches!(result, Err(MediaError::Unsupported)));
    }
}
