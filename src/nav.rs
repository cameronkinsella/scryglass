//! Directory scanning, sorted file list, and wrap-around cursor navigation.

use std::path::{Path, PathBuf};

use crate::config::AppConfig;

#[derive(Debug, thiserror::Error)]
pub enum NavError {
    #[error("directory contains no supported images")]
    EmptyDirectory,
    #[error("start file not found in directory listing")]
    StartFileNotFound,
    #[error("I/O error scanning directory: {0}")]
    Io(#[from] std::io::Error),
}

/// A navigable, circular file list with a cursor.
#[derive(Debug)]
pub struct Nav {
    files: Vec<PathBuf>,
    cursor: usize,
}

impl Nav {
    /// Build a `Nav` from a list of files, seeking the cursor to `start`.
    /// Returns `NavError::EmptyDirectory` if `files` is empty.
    /// Returns `NavError::StartFileNotFound` if `start` is not in the list.
    pub fn new(files: Vec<PathBuf>, start: &Path) -> Result<Self, NavError> {
        if files.is_empty() {
            return Err(NavError::EmptyDirectory);
        }
        let cursor = files
            .iter()
            .position(|f| f == start)
            .ok_or(NavError::StartFileNotFound)?;
        Ok(Self { files, cursor })
    }

    /// Current image path.
    pub fn current(&self) -> &Path {
        &self.files[self.cursor]
    }

    /// Advance forward one image (wraps around).
    pub fn next(&mut self) {
        self.cursor = (self.cursor + 1) % self.files.len();
    }

    /// Go back one image (wraps around).
    pub fn prev(&mut self) {
        self.cursor = (self.cursor + self.files.len() - 1) % self.files.len();
    }

    /// Peek at the next image path without moving the cursor.
    #[allow(dead_code)]
    pub fn peek_next(&self) -> PathBuf {
        let idx = (self.cursor + 1) % self.files.len();
        self.files[idx].clone()
    }

    /// Peek at the previous image path without moving the cursor.
    #[allow(dead_code)]
    pub fn peek_prev(&self) -> PathBuf {
        let idx = (self.cursor + self.files.len() - 1) % self.files.len();
        self.files[idx].clone()
    }

    /// Returns paths surrounding the cursor for pre-fetching.
    /// Returns up to `depth` images in each direction, excluding the current image.
    /// Deduplicates automatically for small lists.
    pub fn peek_around(&self, depth: usize) -> Vec<PathBuf> {
        let len = self.files.len();
        let mut seen = std::collections::HashSet::new();
        seen.insert(self.cursor); // exclude current
        let mut result = Vec::new();

        for offset in 1..=depth {
            let fwd = (self.cursor + offset) % len;
            if seen.insert(fwd) {
                result.push(self.files[fwd].clone());
            }
        }
        for offset in 1..=depth {
            let bwd = (self.cursor + len - (offset % len)) % len;
            if seen.insert(bwd) {
                result.push(self.files[bwd].clone());
            }
        }
        result
    }

    /// Number of files in the list.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the file list is empty (always false after successful construction).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Current cursor index.
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Human-readable position label, e.g. "3/48".
    pub fn position_label(&self) -> String {
        format!("{}/{}", self.cursor + 1, self.files.len())
    }

    /// Access the full file list.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Jump the cursor to an absolute index (wraps via modular arithmetic).
    pub fn set_cursor(&mut self, index: usize) {
        self.cursor = index % self.files.len();
    }
}

/// Scan `dir` for files with supported image extensions, returning a sorted list.
pub fn scan_directory(dir: &Path) -> Result<Vec<PathBuf>, NavError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(AppConfig::is_supported_extension)
        })
        .collect();

    // Natural, case-insensitive ordering: img2 before img10, like file managers.
    files.sort_by(|a, b| {
        natord::compare_ignore_case(
            &a.file_name().unwrap_or_default().to_string_lossy(),
            &b.file_name().unwrap_or_default().to_string_lossy(),
        )
    });

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_files(dir: &Path, names: &[&str]) {
        for name in names {
            fs::write(dir.join(name), b"").unwrap();
        }
    }

    // --- Nav cursor tests ---

    #[test]
    fn new_with_empty_list_returns_error() {
        let result = Nav::new(vec![], Path::new("a.png"));
        assert!(matches!(result, Err(NavError::EmptyDirectory)));
    }

    #[test]
    fn new_with_missing_start_returns_error() {
        let files = vec![PathBuf::from("a.png")];
        let result = Nav::new(files, Path::new("b.png"));
        assert!(matches!(result, Err(NavError::StartFileNotFound)));
    }

    #[test]
    fn cursor_starts_at_correct_file() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let nav = Nav::new(files, Path::new("b.png")).unwrap();
        assert_eq!(nav.current(), Path::new("b.png"));
        assert_eq!(nav.cursor(), 1);
    }

    #[test]
    fn next_advances_cursor() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("a.png")).unwrap();
        nav.next();
        assert_eq!(nav.current(), Path::new("b.png"));
    }

    #[test]
    fn next_wraps_around() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        nav.next();
        assert_eq!(nav.current(), Path::new("a.png"));
    }

    #[test]
    fn prev_goes_backward() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        nav.prev();
        assert_eq!(nav.current(), Path::new("a.png"));
    }

    #[test]
    fn prev_wraps_around() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("a.png")).unwrap();
        nav.prev();
        assert_eq!(nav.current(), Path::new("b.png"));
    }

    #[test]
    fn single_file_wraps_to_itself() {
        let files = vec![PathBuf::from("only.png")];
        let mut nav = Nav::new(files, Path::new("only.png")).unwrap();
        nav.next();
        assert_eq!(nav.current(), Path::new("only.png"));
        nav.prev();
        assert_eq!(nav.current(), Path::new("only.png"));
    }

    // --- peek_around tests ---

    #[test]
    fn peek_around_returns_neighbors() {
        let files: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(format!("{i}.png"))).collect();
        let nav = Nav::new(files, Path::new("3.png")).unwrap();
        let around = nav.peek_around(2);
        // Forward: 4, 5. Backward: 2, 1.
        assert!(around.contains(&PathBuf::from("4.png")));
        assert!(around.contains(&PathBuf::from("5.png")));
        assert!(around.contains(&PathBuf::from("2.png")));
        assert!(around.contains(&PathBuf::from("1.png")));
        assert!(!around.contains(&PathBuf::from("3.png"))); // current excluded
    }

    #[test]
    fn peek_around_wraps_at_boundaries() {
        let files: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!("{i}.png"))).collect();
        let nav = Nav::new(files, Path::new("4.png")).unwrap();
        let around = nav.peek_around(2);
        // Forward (wrapping): 0, 1. Backward: 3, 2.
        assert!(around.contains(&PathBuf::from("0.png")));
        assert!(around.contains(&PathBuf::from("1.png")));
        assert!(around.contains(&PathBuf::from("3.png")));
        assert!(around.contains(&PathBuf::from("2.png")));
    }

    #[test]
    fn peek_around_small_list_no_duplicates() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let nav = Nav::new(files, Path::new("a.png")).unwrap();
        let around = nav.peek_around(3);
        // Only "b.png" is the neighbor in both directions, so it should appear once.
        assert_eq!(around.len(), 1);
        assert!(around.contains(&PathBuf::from("b.png")));
    }

    // --- scan_directory tests ---

    #[test]
    fn scan_directory_filters_non_image_files() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["a.png", "b.txt", "c.jpg", "d.rs"]);
        let files = scan_directory(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.file_name().unwrap() == "a.png"));
        assert!(files.iter().any(|f| f.file_name().unwrap() == "c.jpg"));
    }

    #[test]
    fn scan_directory_sorts_case_insensitively() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["Banana.png", "apple.png", "Cherry.png"]);
        let files = scan_directory(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, &["apple.png", "Banana.png", "Cherry.png"]);
    }

    #[test]
    fn scan_directory_sorts_numbers_naturally() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["img10.png", "img2.png", "img1.png"]);
        let files = scan_directory(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, &["img1.png", "img2.png", "img10.png"]);
    }

    #[test]
    fn scan_directory_natural_sort_is_case_insensitive() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["B1.png", "a10.png", "a2.png"]);
        let files = scan_directory(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, &["a2.png", "a10.png", "B1.png"]);
    }

    #[test]
    fn scan_directory_returns_empty_vec_for_no_images() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["readme.txt", "notes.md"]);
        let files = scan_directory(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn scan_directory_handles_mixed_case_extensions() {
        let dir = TempDir::new().unwrap();
        make_files(dir.path(), &["photo.PNG", "snap.JpG"]);
        let files = scan_directory(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    // --- peek_next / peek_prev tests ---

    #[test]
    fn peek_next_returns_next_without_moving_cursor() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let nav = Nav::new(files, Path::new("a.png")).unwrap();
        assert_eq!(nav.peek_next(), PathBuf::from("b.png"));
        // Cursor should not have moved.
        assert_eq!(nav.current(), Path::new("a.png"));
    }

    #[test]
    fn peek_prev_returns_prev_without_moving_cursor() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let nav = Nav::new(files, Path::new("b.png")).unwrap();
        assert_eq!(nav.peek_prev(), PathBuf::from("a.png"));
        assert_eq!(nav.current(), Path::new("b.png"));
    }

    #[test]
    fn peek_next_wraps_around() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let nav = Nav::new(files, Path::new("b.png")).unwrap();
        assert_eq!(nav.peek_next(), PathBuf::from("a.png"));
    }

    #[test]
    fn peek_prev_wraps_around() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let nav = Nav::new(files, Path::new("a.png")).unwrap();
        assert_eq!(nav.peek_prev(), PathBuf::from("b.png"));
    }
}
