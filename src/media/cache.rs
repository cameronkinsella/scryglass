//! LRU cache with a byte budget, keyed by path.
//!
//! The app stores GPU-resident allocations in here. The cache is generic
//! over the stored value so the eviction logic stays pure and testable.
//! Entries inside the prefetch window are pinned by the caller and never
//! evicted. Eviction only reclaims images the user has scrolled away from.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

struct Entry<T> {
    value: T,
    bytes: usize,
    last_used: u64,
}

pub struct ImageCache<T> {
    entries: HashMap<PathBuf, Entry<T>>,
    clock: u64,
    used_bytes: usize,
    budget: usize,
}

impl<T> ImageCache<T> {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            used_bytes: 0,
            budget: budget_bytes,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Fetch a cached value, marking it as recently used.
    pub fn get(&mut self, path: &Path) -> Option<&T> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(path)?;
        entry.last_used = clock;
        Some(&entry.value)
    }

    /// Fetch without touching recency, for read-only view code.
    pub fn peek(&self, path: &Path) -> Option<&T> {
        self.entries.get(path).map(|e| &e.value)
    }

    /// Remove an entry (file deleted or renamed), returning its value.
    pub fn remove(&mut self, path: &Path) -> Option<T> {
        let entry = self.entries.remove(path)?;
        self.used_bytes = self.used_bytes.saturating_sub(entry.bytes);
        Some(entry.value)
    }

    /// Insert (or replace) a value costing `bytes`.
    pub fn insert(&mut self, path: PathBuf, value: T, bytes: usize) {
        self.clock += 1;
        if let Some(old) = self.entries.insert(
            path,
            Entry {
                value,
                bytes,
                last_used: self.clock,
            },
        ) {
            self.used_bytes -= old.bytes;
        }
        self.used_bytes += bytes;
    }

    /// Evict least-recently-used entries until the budget is met, skipping
    /// pinned paths. If everything over budget is pinned, nothing happens and
    /// the working set always stays resident.
    pub fn evict_over_budget(&mut self, pinned: &HashSet<PathBuf>) {
        while self.used_bytes > self.budget {
            let victim = self
                .entries
                .iter()
                .filter(|(path, _)| !pinned.contains(*path))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(path, _)| path.clone());

            let Some(victim) = victim else {
                return;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.used_bytes -= entry.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn remove_returns_value_and_frees_bytes() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 7, 40);
        assert_eq!(cache.remove(Path::new("a.png")), Some(7));
        assert_eq!(cache.used_bytes(), 0);
        assert_eq!(cache.remove(Path::new("a.png")), None);
    }

    #[test]
    fn insert_accounts_bytes() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 30);
        assert_eq!(cache.used_bytes(), 70);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn replacing_an_entry_does_not_double_count() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("a.png".into(), 2, 50);
        assert_eq!(cache.used_bytes(), 50);
        assert_eq!(cache.get(Path::new("a.png")), Some(&2));
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 40);
        // Touch a so b becomes the LRU entry.
        cache.get(Path::new("a.png"));
        cache.insert("c.png".into(), 3, 40); // 120 > 100

        cache.evict_over_budget(&pins(&[]));
        assert!(cache.contains(Path::new("a.png")));
        assert!(!cache.contains(Path::new("b.png")));
        assert!(cache.contains(Path::new("c.png")));
        assert_eq!(cache.used_bytes(), 80);
    }

    #[test]
    fn pinned_entries_survive_eviction() {
        let mut cache: ImageCache<u8> = ImageCache::new(50);
        cache.insert("old.png".into(), 1, 40);
        cache.insert("new.png".into(), 2, 40);

        cache.evict_over_budget(&pins(&["old.png", "new.png"]));
        assert_eq!(cache.len(), 2, "working set must stay resident");

        cache.evict_over_budget(&pins(&["new.png"]));
        assert!(!cache.contains(Path::new("old.png")));
        assert!(cache.contains(Path::new("new.png")));
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.evict_over_budget(&pins(&[]));
        assert_eq!(cache.len(), 1);
    }
}
