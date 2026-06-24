//! Directory scanning, sorted file list, and wrap-around cursor navigation.

use std::path::{Path, PathBuf};

use crate::config::{AppConfig, SortKey};

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
    #[allow(dead_code)]
    pub fn next(&mut self) {
        self.cursor = (self.cursor + 1) % self.files.len();
    }

    /// Go back one image (wraps around).
    #[allow(dead_code)]
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

    /// Swap in a re-ordered file list, keeping the cursor on the same
    /// file when it's still present. Empty lists are ignored.
    pub fn replace_files(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        let current = self.current().to_path_buf();
        self.cursor = files.iter().position(|p| *p == current).unwrap_or(0);
        self.files = files;
    }

    /// Remove a file from the list (it was deleted). The cursor stays at
    /// the same position, which now points at the next file (or the new
    /// last file). Returns false when the list became empty.
    pub fn remove(&mut self, path: &Path) -> bool {
        if let Some(index) = self.files.iter().position(|p| p == path) {
            self.files.remove(index);
            if index < self.cursor {
                self.cursor -= 1;
            }
            if !self.files.is_empty() {
                self.cursor = self.cursor.min(self.files.len() - 1);
            }
        }
        !self.files.is_empty()
    }

    /// Update a file's path in place (it was renamed).
    pub fn rename(&mut self, old: &Path, new: PathBuf) {
        if let Some(index) = self.files.iter().position(|p| p == old) {
            self.files[index] = new;
        }
    }
}

/// Metadata needed to sort a file list.
pub struct FileMeta {
    pub path: PathBuf,
    pub modified: Option<std::time::SystemTime>,
    pub size: u64,
}

/// Compare file names the way the platform's file manager does:
/// StrCmpLogicalW on Windows (Explorer's ordering), a natural
/// case-insensitive compare elsewhere.
#[cfg(windows)]
pub fn name_cmp(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> std::cmp::Ordering {
    use std::os::windows::ffi::OsStrExt;
    let a_wide: Vec<u16> = a.encode_wide().chain(std::iter::once(0)).collect();
    let b_wide: Vec<u16> = b.encode_wide().chain(std::iter::once(0)).collect();
    let order =
        unsafe { windows_sys::Win32::UI::Shell::StrCmpLogicalW(a_wide.as_ptr(), b_wide.as_ptr()) };
    order.cmp(&0)
}

#[cfg(not(windows))]
pub fn name_cmp(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> std::cmp::Ordering {
    natord::compare_ignore_case(&a.to_string_lossy(), &b.to_string_lossy())
}

/// Order files by the configured key. Ties (and missing metadata) fall
/// back to name order, so results are always deterministic.
pub fn sort_paths(mut entries: Vec<FileMeta>, key: SortKey, descending: bool) -> Vec<PathBuf> {
    let by_name = |a: &FileMeta, b: &FileMeta| {
        name_cmp(
            a.path.file_name().unwrap_or_default(),
            b.path.file_name().unwrap_or_default(),
        )
    };

    entries.sort_by(|a, b| match key {
        SortKey::Name => by_name(a, b),
        SortKey::DateModified => a.modified.cmp(&b.modified).then_with(|| by_name(a, b)),
        SortKey::Size => a.size.cmp(&b.size).then_with(|| by_name(a, b)),
    });
    if descending {
        entries.reverse();
    }
    entries.into_iter().map(|e| e.path).collect()
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

    // Same ordering as the platform's file manager.
    files.sort_by(|a, b| {
        name_cmp(
            a.file_name().unwrap_or_default(),
            b.file_name().unwrap_or_default(),
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

    // --- replace_files / sort_paths tests ---

    #[test]
    fn remove_current_advances_to_next() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        assert!(nav.remove(Path::new("b.png")));
        assert_eq!(nav.current(), Path::new("c.png"));
    }

    #[test]
    fn remove_last_file_steps_back() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        assert!(nav.remove(Path::new("b.png")));
        assert_eq!(nav.current(), Path::new("a.png"));
    }

    #[test]
    fn remove_only_file_reports_empty() {
        let files = vec![PathBuf::from("a.png")];
        let mut nav = Nav::new(files, Path::new("a.png")).unwrap();
        assert!(!nav.remove(Path::new("a.png")));
    }

    #[test]
    fn remove_before_cursor_keeps_current() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let mut nav = Nav::new(files, Path::new("c.png")).unwrap();
        assert!(nav.remove(Path::new("a.png")));
        assert_eq!(nav.current(), Path::new("c.png"));
    }

    #[test]
    fn rename_updates_path_in_place() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("a.png")).unwrap();
        nav.rename(Path::new("a.png"), PathBuf::from("z.png"));
        assert_eq!(nav.current(), Path::new("z.png"));
    }

    #[test]
    fn scan_directory_lists_supported_files_sorted() {
        let dir = std::env::temp_dir().join(format!("scryglass-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.png"), b"x").unwrap();
        std::fs::write(dir.join("a.png"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let names: Vec<String> = scan_directory(&dir)
            .unwrap()
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, [String::from("a.png"), String::from("b.png")]);
    }

    #[test]
    fn replace_files_keeps_cursor_on_current() {
        let files = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        nav.replace_files(vec![
            PathBuf::from("c.png"),
            PathBuf::from("b.png"),
            PathBuf::from("a.png"),
        ]);
        assert_eq!(nav.current(), Path::new("b.png"));
        assert_eq!(nav.cursor(), 1);
    }

    #[test]
    fn replace_files_falls_back_to_first_when_current_gone() {
        let files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let mut nav = Nav::new(files, Path::new("b.png")).unwrap();
        nav.replace_files(vec![PathBuf::from("x.png"), PathBuf::from("y.png")]);
        assert_eq!(nav.current(), Path::new("x.png"));
    }

    fn meta(name: &str, secs: u64, size: u64) -> FileMeta {
        FileMeta {
            path: PathBuf::from(name),
            modified: Some(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            ),
            size,
        }
    }

    #[test]
    fn sort_by_date_then_descending() {
        let entries = vec![meta("new.png", 300, 1), meta("old.png", 100, 1)];
        let sorted = sort_paths(entries, SortKey::DateModified, false);
        assert_eq!(sorted, [PathBuf::from("old.png"), PathBuf::from("new.png")]);

        let entries = vec![meta("new.png", 300, 1), meta("old.png", 100, 1)];
        let sorted = sort_paths(entries, SortKey::DateModified, true);
        assert_eq!(sorted, [PathBuf::from("new.png"), PathBuf::from("old.png")]);
    }

    #[test]
    fn sort_by_size_with_name_tiebreak() {
        let entries = vec![
            meta("b.png", 0, 50),
            meta("a.png", 0, 50),
            meta("big.png", 0, 900),
        ];
        let sorted = sort_paths(entries, SortKey::Size, false);
        assert_eq!(
            sorted,
            [
                PathBuf::from("a.png"),
                PathBuf::from("b.png"),
                PathBuf::from("big.png")
            ]
        );
    }

    #[test]
    fn name_sort_orders_numeric_runs_by_value() {
        let entries = vec![meta("img10.png", 0, 0), meta("img2.png", 0, 0)];
        let sorted = sort_paths(entries, SortKey::Name, false);
        assert_eq!(
            sorted,
            [PathBuf::from("img2.png"), PathBuf::from("img10.png")]
        );
    }

    // Explorer puts punctuation before digits. This is the ordering the
    // user actually sees in the shell, so match it exactly.
    #[cfg(windows)]
    #[test]
    fn name_sort_matches_explorer_punctuation_order() {
        let entries = vec![
            meta("0a.png", 0, 0),
            meta("_b.png", 0, 0),
            meta("[c.png", 0, 0),
        ];
        let sorted = sort_paths(entries, SortKey::Name, false);
        assert_eq!(
            sorted,
            [
                PathBuf::from("[c.png"),
                PathBuf::from("_b.png"),
                PathBuf::from("0a.png")
            ]
        );
    }
}
