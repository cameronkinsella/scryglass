//! Byte-budgeted LRU cache, keyed by path.
//!
//! The app uses it for the thumbnail cache shared across windows. The cache is
//! generic over the stored value so the eviction logic stays pure and testable.
//! The budget is enforced on every insert, so a background thumbnailer walking
//! a whole directory can never grow the cache unbounded. Paths in the pinned
//! set (the caller's working set, refreshed on navigation) are never evicted.
//! Eviction only reclaims images the user has scrolled away from.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

struct Entry<T> {
    value: T,
    bytes: usize,
    last_used: u64,
}

pub struct ImageCache<T> {
    entries: HashMap<PathBuf, Entry<T>>,
    /// Eviction order: insertion clock to path, oldest first. Clocks are
    /// unique, so this doubles as the LRU queue and keeps eviction at
    /// O(victims log n) instead of a full map scan per victim.
    order: BTreeMap<u64, PathBuf>,
    /// Paths that never evict, replaced wholesale via [`Self::set_pinned`].
    pinned: HashSet<PathBuf>,
    clock: u64,
    used_bytes: usize,
    budget: usize,
}

impl<T> ImageCache<T> {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            pinned: HashSet::new(),
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

    /// Fetch a cached value (recency is tracked by insertion, not access, since
    /// the render paths read through `peek`).
    pub fn peek(&self, path: &Path) -> Option<&T> {
        self.entries.get(path).map(|e| &e.value)
    }

    /// Remove an entry (file deleted or renamed), returning its value.
    pub fn remove(&mut self, path: &Path) -> Option<T> {
        let entry = self.entries.remove(path)?;
        self.order.remove(&entry.last_used);
        self.used_bytes = self.used_bytes.saturating_sub(entry.bytes);
        Some(entry.value)
    }

    /// Insert (or replace) a value costing `bytes`, then evict
    /// least-recently-used unpinned entries until the budget is met.
    pub fn insert(&mut self, path: PathBuf, value: T, bytes: usize) {
        self.clock += 1;
        self.order.insert(self.clock, path.clone());
        if let Some(old) = self.entries.insert(
            path,
            Entry {
                value,
                bytes,
                last_used: self.clock,
            },
        ) {
            self.order.remove(&old.last_used);
            self.used_bytes -= old.bytes;
        }
        self.used_bytes += bytes;
        self.enforce_budget();
    }

    /// Replace the pinned set (the paths that must survive eviction: the
    /// on-screen working set) and reclaim over-budget entries right away.
    pub fn set_pinned(&mut self, pinned: HashSet<PathBuf>) {
        self.pinned = pinned;
        self.enforce_budget();
    }

    /// Evict least-recently-used unpinned entries until the budget is met.
    /// If everything over budget is pinned, nothing happens and the working
    /// set always stays resident.
    fn enforce_budget(&mut self) {
        if self.used_bytes <= self.budget {
            return;
        }
        let mut victims = Vec::new();
        let mut projected = self.used_bytes;
        for (clock, path) in &self.order {
            if projected <= self.budget {
                break;
            }
            if self.pinned.contains(path) {
                continue;
            }
            projected -= self.entries[path].bytes;
            victims.push(*clock);
        }
        for clock in victims {
            let Some(path) = self.order.remove(&clock) else {
                continue;
            };
            if let Some(entry) = self.entries.remove(&path) {
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
        assert_eq!(cache.peek(Path::new("a.png")), Some(&2));
    }

    #[test]
    fn insert_past_budget_evicts_the_oldest_first() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 40);
        cache.insert("c.png".into(), 3, 40); // 120 > 100

        // The budget is enforced by the insert itself, no separate sweep.
        // Recency is by insertion, so the oldest entry is dropped first.
        assert!(!cache.contains(Path::new("a.png")));
        assert!(cache.contains(Path::new("b.png")));
        assert!(cache.contains(Path::new("c.png")));
        assert_eq!(cache.used_bytes(), 80);
    }

    #[test]
    fn replacing_an_entry_refreshes_its_recency() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 40);
        cache.insert("a.png".into(), 3, 40);
        // "b.png" is now the oldest, so it goes first.
        cache.insert("c.png".into(), 4, 40);
        assert!(cache.contains(Path::new("a.png")));
        assert!(!cache.contains(Path::new("b.png")));
        assert!(cache.contains(Path::new("c.png")));
    }

    #[test]
    fn removed_entries_leave_no_stale_eviction_order() {
        let mut cache: ImageCache<u8> = ImageCache::new(80);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 40);
        cache.remove(Path::new("a.png"));
        // Room for one more without touching "b.png".
        cache.insert("c.png".into(), 3, 40);
        assert!(cache.contains(Path::new("b.png")));
        assert!(cache.contains(Path::new("c.png")));
        assert_eq!(cache.used_bytes(), 80);
    }

    #[test]
    fn pinned_entries_survive_eviction() {
        let mut cache: ImageCache<u8> = ImageCache::new(50);
        cache.insert("old.png".into(), 1, 40);
        cache.set_pinned(pins(&["old.png", "new.png"]));
        cache.insert("new.png".into(), 2, 40);
        assert_eq!(cache.len(), 2, "working set must stay resident");

        // Unpinning evicts over-budget entries right away, oldest first.
        cache.set_pinned(pins(&["new.png"]));
        assert!(!cache.contains(Path::new("old.png")));
        assert!(cache.contains(Path::new("new.png")));
    }

    #[test]
    fn an_unpinned_insert_evicts_itself_when_pins_fill_the_budget() {
        let mut cache: ImageCache<u8> = ImageCache::new(50);
        cache.insert("pinned.png".into(), 1, 40);
        cache.set_pinned(pins(&["pinned.png"]));
        cache.insert("extra.png".into(), 2, 40);
        // Nothing older is evictable, so the newcomer itself goes.
        assert!(cache.contains(Path::new("pinned.png")));
        assert!(!cache.contains(Path::new("extra.png")));
        assert_eq!(cache.used_bytes(), 40);
    }

    #[test]
    fn eviction_skips_pinned_and_takes_the_next_oldest() {
        let mut cache: ImageCache<u8> = ImageCache::new(120);
        cache.insert("a.png".into(), 1, 40);
        cache.insert("b.png".into(), 2, 40);
        cache.insert("c.png".into(), 3, 40);
        cache.set_pinned(pins(&["a.png"]));
        cache.insert("d.png".into(), 4, 40); // 160 > 120
        // "a.png" is oldest but pinned; "b.png" is the next oldest.
        assert!(cache.contains(Path::new("a.png")));
        assert!(!cache.contains(Path::new("b.png")));
        assert!(cache.contains(Path::new("c.png")));
        assert!(cache.contains(Path::new("d.png")));
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let mut cache: ImageCache<u8> = ImageCache::new(100);
        cache.insert("a.png".into(), 1, 40);
        cache.set_pinned(pins(&[]));
        assert_eq!(cache.len(), 1);
    }
}
