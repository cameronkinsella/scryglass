//! The single texture-lifecycle store: the one owner of every still image's
//! decoded RAM and GPU texture, keyed by image identity and shared across all
//! windows. This module is the typestate core; it is wired into the load,
//! display, prefetch, and decay paths over the migration stages that follow, so
//! some items are not referenced yet.
//!
//! Three invariants are made structural here rather than checked by hand:
//!
//! - **One texture per image, never duplicated.** Identity is [`ImageKey`]
//!   (container + entry), not a per-decode handle id. The store is the only
//!   minter; holders receive `Arc` clones of the one texture, so cloning is a
//!   refcount bump, not a copy.
//! - **No invalid tier move.** The tier values ([`FullTexture`], [`ViewTexture`],
//!   [`RamImage`], [`Evicted`]) expose only forward-release transitions, each
//!   consuming `self`; a backward move like evicted -> demote has no method, so
//!   it does not compile.
//! - **Demote/evict only once every holder permits.** Demand is a set of atomic
//!   per-tier counters ([`TierCounters`]) shared between the store entry and every
//!   lease; the image sits at `max_wanted()`, recomputed O(1) on each lease event,
//!   never by scanning windows.

use std::cell::Cell as StdCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use iced::widget::image::Handle;

use crate::media::pipeline::{Source, cache_key};
use crate::ui::image_surface::{Keepalive, ResidentImage};

/// Identity of an image in the store: the dedup container (folder, or archive
/// path) and the entry within it. Deliberately not `Handle::id`, which is minted
/// fresh on every decode, so promote/demote/rotate of one image would otherwise
/// each look like a different image.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ImageKey {
    container: PathBuf,
    entry: OsString,
}

impl ImageKey {
    pub fn new(source: &Source, path: &Path) -> Self {
        let (container, entry) = cache_key(source, path);
        Self { container, entry }
    }
}

/// A still image's resource tier, least-resident first. The store keeps each image
/// at the highest tier any holder currently demands; `Ord` is what makes that a
/// one-line `max`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    Evicted = 0,
    InRam = 1,
    View = 2,
    Full = 3,
}

impl Tier {
    const ALL: [Tier; 4] = [Tier::Evicted, Tier::InRam, Tier::View, Tier::Full];
}

// --- typestate tier values: forward-release transitions consume self; the
// invalid (backward) moves simply have no method, so they cannot be written. ---

/// Full-resolution RGBA in RAM, the substrate every GPU tier is uploaded from.
/// Arc-backed through iced's `Handle`, so a clone is a refcount bump and the last
/// drop frees the RAM.
#[derive(Clone, Debug)]
pub struct RamImage {
    pub handle: Handle,
    pub original_size: (u32, u32),
    /// Read + decode time, for the dynamic eviction policy.
    pub decode_time: Option<Duration>,
}

/// A view-resolution GPU texture, plus the RAM to re-derive from. A rendering
/// subset of [`FullTexture`].
pub struct ViewTexture {
    pub ram: RamImage,
    pub texture: Keepalive,
}

/// A full-resolution GPU texture, plus its RAM.
pub struct FullTexture {
    pub ram: RamImage,
    pub texture: Keepalive,
}

/// Nothing resident; re-acquiring requires a decode.
pub struct Evicted;

impl FullTexture {
    /// Release the GPU texture, keeping the RAM source. The texture's VRAM frees
    /// when the last clone of `texture` drops.
    pub fn drop_to_ram(self) -> RamImage {
        self.ram
    }
}

impl ViewTexture {
    /// Release the GPU texture, keeping the RAM source.
    pub fn drop_to_ram(self) -> RamImage {
        self.ram
    }
}

impl RamImage {
    /// Release the RAM source. There is no way back without a fresh decode.
    pub fn evict(self) -> Evicted {
        Evicted
    }
}

// `Evicted` has no `drop_to_ram`/`evict`/demote: a backward or skipping move such
// as `evicted.drop_to_ram()` references a method that does not exist and fails to
// compile. That is the whole "no invalid transition" guarantee.

/// The tier value at rest inside a store entry. A keyed map needs one concrete
/// type, so the typestate is erased into this enum between transitions; the
/// consuming-`self` safety still lives on the values above, where the transitions
/// are written.
pub enum CellState {
    Evicted,
    InRam(RamImage),
    View(ViewTexture),
    Full(FullTexture),
}

impl CellState {
    pub fn tier(&self) -> Tier {
        match self {
            CellState::Evicted => Tier::Evicted,
            CellState::InRam(_) => Tier::InRam,
            CellState::View(_) => Tier::View,
            CellState::Full(_) => Tier::Full,
        }
    }

    /// The resident texture to hand to holders, if any tier currently has one.
    pub fn texture(&self) -> Option<Keepalive> {
        match self {
            CellState::Full(f) => Some(f.texture.clone()),
            CellState::View(v) => Some(v.texture.clone()),
            CellState::InRam(_) | CellState::Evicted => None,
        }
    }

    /// The RAM source, if resident at any tier above eviction (cheap: an `Arc`
    /// bump on the handle), so an upload can re-derive a texture without a decode.
    pub fn ram(&self) -> Option<RamImage> {
        match self {
            CellState::Full(f) => Some(f.ram.clone()),
            CellState::View(v) => Some(v.ram.clone()),
            CellState::InRam(r) => Some(r.clone()),
            CellState::Evicted => None,
        }
    }
}

/// One swappable texture slot per image, shared (`Arc`) by every holder. The store
/// swaps it once per tier change — O(1), with no fan-out over holders — and every
/// holder reads it at render time. Empty while the image is still decoding, so the
/// display falls back to its thumbnail blur (never a black frame).
#[derive(Default)]
pub struct TextureCell {
    texture: arc_swap::ArcSwapOption<ResidentImage>,
}

impl TextureCell {
    /// The current shared texture, or `None` while pending / not resident.
    pub fn load(&self) -> Option<Keepalive> {
        self.texture.load_full()
    }

    /// Install (or clear) the shared texture. The previous one frees once its last
    /// clone drops.
    pub fn store(&self, texture: Option<Keepalive>) {
        self.texture.store(texture);
    }
}

/// Per-tier demand counters, shared (`Arc`) between a store entry and every lease
/// of that image. The image's tier is the highest tier with a live demand; this is
/// the entire cross-window aggregation, kept O(1) by lease create/drop/downgrade.
#[derive(Default)]
pub struct TierCounters {
    want: [AtomicU32; 4],
}

impl TierCounters {
    pub fn inc(&self, tier: Tier) {
        self.want[tier as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self, tier: Tier) {
        self.want[tier as usize].fetch_sub(1, Ordering::Relaxed);
    }

    /// The highest tier any live holder demands, or `Evicted` if none do. O(1): a
    /// fixed four-slot scan, never over windows.
    pub fn max_wanted(&self) -> Tier {
        Tier::ALL
            .into_iter()
            .rev()
            .find(|t| self.want[*t as usize].load(Ordering::Relaxed) > 0)
            .unwrap_or(Tier::Evicted)
    }
}

/// Async work the store needs the app to run; the result returns via
/// [`Store::on_decoded`]/[`Store::on_minted`]. The store itself does no I/O or GPU
/// work, so its decisions stay pure and unit-testable.
pub enum Job {
    /// Decode `path` from `source` to RAM, then call [`Store::on_decoded`].
    Decode {
        key: ImageKey,
        path: PathBuf,
        source: Source,
    },
    /// Upload the held `ram` for `key` at `tier` (full, or downscaled to view),
    /// then call [`Store::on_minted`].
    Upload {
        key: ImageKey,
        tier: Tier,
        ram: RamImage,
    },
}

#[derive(Default)]
pub struct StoreOutcome {
    pub jobs: Vec<Job>,
}

/// Keys whose demand changed via a `Lease` drop, drained by [`Store::pump`]. A drop
/// can't run store logic (no `&mut Store`, can't be async), so it just records the
/// key here; the next pump reconciles it. O(keys dirtied), never a scan.
type Dirty = Arc<Mutex<Vec<ImageKey>>>;

/// A holder's claim on an image at a tier. Held in window state (a display slot or a
/// prefetch slot). Dropping it lowers the demand by one, RAII, with no store call.
pub struct Lease {
    key: ImageKey,
    want: StdCell<Tier>,
    counters: Arc<TierCounters>,
    cell: Arc<TextureCell>,
    dirty: Dirty,
}

impl Lease {
    /// The shared texture slot to read at render time (or `None` while pending).
    pub fn texture(&self) -> Option<Keepalive> {
        self.cell.load()
    }

    /// This holder's current demand.
    pub fn want(&self) -> Tier {
        self.want.get()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.counters.dec(self.want.get());
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.push(self.key.clone());
        }
    }
}

struct Entry {
    counters: Arc<TierCounters>,
    /// Holders own the strong `Arc<TextureCell>`; the entry only borrows it. A dead
    /// weak therefore means no holder is leasing this image right now.
    cell: Weak<TextureCell>,
    state: CellState,
    /// How to re-decode this image, kept so any window's request can drive it.
    path: PathBuf,
    source: Source,
    /// The tier a decode/upload is currently in flight for, so a reconcile does not
    /// fire a duplicate job while one is pending.
    pending: Option<Tier>,
}

impl Entry {
    fn is_leased(&self) -> bool {
        self.cell.strong_count() > 0
    }
}

/// The single texture-lifecycle store: the one owner of every still image's tier,
/// keyed by [`ImageKey`]. Holders talk to it through [`Lease`]s; it answers with the
/// async [`Job`]s the app should run. All decisions are O(1) (a hash lookup and the
/// fixed four-slot `max_wanted`); nothing iterates windows or holders.
pub struct Store {
    entries: HashMap<ImageKey, Entry>,
    dirty: Dirty,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            dirty: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Store {
    /// A holder claims `key` at `want`. Returns a [`Lease`] (its `texture()` is
    /// `None` until the tier is resident) and any work to start. O(1).
    pub fn request(
        &mut self,
        key: ImageKey,
        path: PathBuf,
        source: Source,
        want: Tier,
    ) -> (Lease, StoreOutcome) {
        let dirty = self.dirty.clone();
        let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
            counters: Arc::new(TierCounters::default()),
            cell: Weak::new(),
            state: CellState::Evicted,
            path,
            source,
            pending: None,
        });
        entry.counters.inc(want);
        // Reuse the live shared cell, or seed a fresh one with whatever is resident.
        let cell = entry.cell.upgrade().unwrap_or_else(|| {
            let cell = Arc::new(TextureCell::default());
            cell.store(entry.state.texture());
            entry.cell = Arc::downgrade(&cell);
            cell
        });
        let lease = Lease {
            key: key.clone(),
            want: StdCell::new(want),
            counters: entry.counters.clone(),
            cell,
            dirty,
        };
        let outcome = self.reconcile(&key);
        (lease, outcome)
    }

    /// Lower (or raise) a holder's demand in place, e.g. a decay step. O(1).
    pub fn retarget(&mut self, lease: &Lease, to: Tier) -> StoreOutcome {
        lease.counters.dec(lease.want.get());
        lease.counters.inc(to);
        lease.want.set(to);
        self.reconcile(&lease.key)
    }

    /// A decode finished: the image is now in RAM. Drive it on toward demand.
    pub fn on_decoded(&mut self, key: ImageKey, ram: RamImage) -> StoreOutcome {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pending = None;
            if matches!(entry.state, CellState::Evicted) {
                entry.state = CellState::InRam(ram);
            }
        }
        self.reconcile(&key)
    }

    /// A decode never produced a still: it was cancelled by a newer navigation
    /// (`retry` = keep the entry and re-emit a decode if still wanted) or it
    /// failed / turned out to be an animation (`retry` false = forget it). Clears
    /// the in-flight marker either way so the store is not stuck pending.
    pub fn on_decode_failed(&mut self, key: &ImageKey, retry: bool) -> StoreOutcome {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.pending = None;
        }
        if retry {
            self.reconcile(key)
        } else {
            self.abandon(key);
            StoreOutcome::default()
        }
    }

    /// An upload finished: install the texture at `tier` and swap the shared cell.
    pub fn on_minted(&mut self, key: ImageKey, tier: Tier, texture: Keepalive) -> StoreOutcome {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pending = None;
            let ram = entry.state.ram();
            if let Some(ram) = ram {
                entry.state = match tier {
                    Tier::Full => CellState::Full(FullTexture { ram, texture }),
                    _ => CellState::View(ViewTexture { ram, texture }),
                };
                if let Some(cell) = entry.cell.upgrade() {
                    cell.store(entry.state.texture());
                }
            }
        }
        self.reconcile(&key)
    }

    /// Forget an image entirely: a decode revealed an animation (not a still) or
    /// failed, so the store should no longer track it. Any outstanding lease goes
    /// inert (its cell reads `None` forever); a later request re-creates the entry
    /// and re-decodes. O(1).
    pub fn abandon(&mut self, key: &ImageKey) {
        self.entries.remove(key);
    }

    /// The image's full-res RAM source, if resident at any tier above eviction.
    /// A clone is a refcount bump on the handle. The display reads its `handle`
    /// and `original_size` from here (the texture comes from the lease's cell),
    /// so a second window can show a resident image with no decode of its own.
    pub fn ram(&self, key: &ImageKey) -> Option<RamImage> {
        self.entries.get(key).and_then(|e| e.state.ram())
    }

    /// The image's current resident tier, or `Evicted` if the store is not
    /// tracking it.
    pub fn tier(&self, key: &ImageKey) -> Tier {
        self.entries
            .get(key)
            .map_or(Tier::Evicted, |e| e.state.tier())
    }

    /// The measured read + decode time for this image, feeding the dynamic
    /// eviction delay. `None` if the image is not resident or was never timed
    /// (e.g. reused from another window's decode).
    pub fn decode_time(&self, key: &ImageKey) -> Option<Duration> {
        let entry = self.entries.get(key)?;
        match &entry.state {
            CellState::Full(f) => f.ram.decode_time,
            CellState::View(v) => v.ram.decode_time,
            CellState::InRam(r) => r.decode_time,
            CellState::Evicted => None,
        }
    }

    /// An upload could not reach the GPU (the upload thread was not ready). Clear
    /// the in-flight marker and reconcile, so the next pass re-attempts the mint
    /// if the tier is still wanted.
    pub fn on_mint_failed(&mut self, key: &ImageKey) -> StoreOutcome {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.pending = None;
        }
        self.reconcile(key)
    }

    /// Reconcile every image whose demand changed via a dropped lease. O(keys dirty).
    pub fn pump(&mut self) -> StoreOutcome {
        let keys = match self.dirty.lock() {
            Ok(mut d) => std::mem::take(&mut *d),
            Err(_) => return StoreOutcome::default(),
        };
        let mut outcome = StoreOutcome::default();
        for key in keys {
            outcome.jobs.extend(self.reconcile(&key).jobs);
        }
        outcome
    }

    /// Bring one image's tier in line with `max_wanted`: free downward right away
    /// (synchronous typestate transitions), or emit the one job to acquire upward.
    /// The whole "which tier" decision is this `max_wanted` compare.
    fn reconcile(&mut self, key: &ImageKey) -> StoreOutcome {
        let Some(entry) = self.entries.get_mut(key) else {
            return StoreOutcome::default();
        };
        let want = entry.counters.max_wanted();
        let have = entry.state.tier();

        if want == have {
            return StoreOutcome::default();
        }

        if want < have {
            // Freeing toward a lower tier is synchronous, EXCEPT demoting a full
            // texture to view, which is a re-upload at a smaller size (handled as an
            // acquire below). Full/View -> InRam/Evicted just drops the GPU/RAM.
            if want == Tier::View && have == Tier::Full {
                return self.acquire(key, Tier::View);
            }
            let state = std::mem::replace(&mut entry.state, CellState::Evicted);
            entry.state = demote_to(state, want);
            if let Some(cell) = entry.cell.upgrade() {
                cell.store(entry.state.texture());
            }
            // A fully-evicted, unleased image is forgotten (a later request re-decodes).
            if matches!(entry.state, CellState::Evicted) && !entry.is_leased() {
                self.entries.remove(key);
            }
            return StoreOutcome::default();
        }

        // want > have: acquire the wanted tier.
        self.acquire(key, want)
    }

    /// Emit the single job to move an image up to `target` (decode if evicted, else
    /// upload from the RAM it already holds), unless one is already in flight.
    fn acquire(&mut self, key: &ImageKey, target: Tier) -> StoreOutcome {
        let Some(entry) = self.entries.get_mut(key) else {
            return StoreOutcome::default();
        };
        if entry.pending.is_some_and(|p| p >= target) {
            return StoreOutcome::default();
        }
        let job = match entry.state.ram() {
            None => Job::Decode {
                key: key.clone(),
                path: entry.path.clone(),
                source: entry.source.clone(),
            },
            Some(ram) => Job::Upload {
                key: key.clone(),
                tier: target,
                ram,
            },
        };
        entry.pending = Some(target);
        StoreOutcome { jobs: vec![job] }
    }
}

/// Free an owned tier value down to `target` through the synchronous typestate
/// transitions (full/view drop their texture to RAM; RAM evicts). Never produces a
/// tier above its input, so the pipeline only ever runs forward.
fn demote_to(mut state: CellState, target: Tier) -> CellState {
    while state.tier() > target {
        state = match state {
            CellState::Full(f) => CellState::InRam(f.drop_to_ram()),
            CellState::View(v) => CellState::InRam(v.drop_to_ram()),
            CellState::InRam(r) => {
                r.evict(); // consumes the RAM source, freeing it
                CellState::Evicted
            }
            CellState::Evicted => CellState::Evicted,
        };
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ram() -> RamImage {
        RamImage {
            handle: Handle::from_rgba(2, 2, vec![0u8; 16]),
            original_size: (2, 2),
            decode_time: None,
        }
    }

    #[test]
    fn tier_orders_least_to_most_resident() {
        assert!(Tier::Evicted < Tier::InRam);
        assert!(Tier::InRam < Tier::View);
        assert!(Tier::View < Tier::Full);
    }

    #[test]
    fn max_wanted_is_the_highest_live_demand() {
        let counters = TierCounters::default();
        assert_eq!(counters.max_wanted(), Tier::Evicted);

        // A focused window wants Full, a backgrounded one View: the image is Full.
        counters.inc(Tier::Full);
        counters.inc(Tier::View);
        assert_eq!(counters.max_wanted(), Tier::Full);

        // The focused window leaves: it falls to the next demand, not below it —
        // "demote only once every holder permits", straight from the counts.
        counters.dec(Tier::Full);
        assert_eq!(counters.max_wanted(), Tier::View);

        counters.dec(Tier::View);
        assert_eq!(counters.max_wanted(), Tier::Evicted);
    }

    #[test]
    fn forward_transitions_chain_and_consume_self() {
        let full = FullTexture {
            ram: ram(),
            texture: crate::ui::image_surface::test_keepalive(),
        };
        // Full -> InRam -> Evicted, each move consuming the previous value.
        let in_ram = full.drop_to_ram();
        let _evicted = in_ram.evict();
        // `_evicted.drop_to_ram()` / `.evict()` would not compile: `Evicted` has no
        // such method, so a backward or tier-skipping move is unrepresentable.
    }

    #[test]
    fn cell_state_reports_its_tier_and_texture() {
        let keep = crate::ui::image_surface::test_keepalive();
        let full = CellState::Full(FullTexture {
            ram: ram(),
            texture: keep,
        });
        assert_eq!(full.tier(), Tier::Full);
        assert!(full.texture().is_some());

        let in_ram = CellState::InRam(ram());
        assert_eq!(in_ram.tier(), Tier::InRam);
        assert!(in_ram.texture().is_none());
    }

    #[test]
    fn texture_cell_starts_empty_and_swaps_in_place() {
        let cell = TextureCell::default();
        assert!(cell.load().is_none()); // pending -> the display shows its thumb

        cell.store(Some(crate::ui::image_surface::test_keepalive()));
        assert!(cell.load().is_some());

        cell.store(None);
        assert!(cell.load().is_none());
    }

    fn key() -> ImageKey {
        ImageKey::new(&Source::Fs, Path::new("a.png"))
    }

    fn keep() -> Keepalive {
        crate::ui::image_surface::test_keepalive()
    }

    /// Request `a.png` at Full and drive it through decode + upload to resident,
    /// asserting the store asked for exactly that work. Returns the live store and
    /// its one holder.
    fn resident_full() -> (Store, Lease) {
        let mut store = Store::default();
        let (lease, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
        assert!(lease.texture().is_none()); // pending -> thumb until resident

        let out = store.on_decoded(key(), ram());
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::Full,
                ..
            }]
        ));

        let out = store.on_minted(key(), Tier::Full, keep());
        assert!(out.jobs.is_empty());
        assert!(lease.texture().is_some()); // the held cell now carries the texture
        (store, lease)
    }

    #[test]
    fn an_evicted_image_decodes_then_uploads_to_resident() {
        let _ = resident_full();
    }

    #[test]
    fn an_image_demotes_only_after_every_holder_releases_it() {
        let mut store = Store::default();
        let (a, _) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        let (b, _) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        store.on_decoded(key(), ram());
        store.on_minted(key(), Tier::Full, keep());
        assert!(a.texture().is_some());
        assert!(b.texture().is_some()); // both windows share the one texture

        // One holder lets go: still wanted full, so nothing is freed or re-minted.
        drop(a);
        assert!(store.pump().jobs.is_empty());
        assert!(b.texture().is_some());

        // The last holder lets go: now nobody wants it, so it is evicted and a fresh
        // request has to decode again — proving the texture was actually released.
        drop(b);
        let _ = store.pump();
        let (_c, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
    }

    #[test]
    fn lowering_demand_to_view_re_uploads_at_view_resolution() {
        let (mut store, lease) = resident_full();
        // The only holder decays Full -> View: the store re-uploads a smaller
        // texture rather than just dropping (a view tier is a downscale, not a free).
        let out = store.retarget(&lease, Tier::View);
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::View,
                ..
            }]
        ));
    }

    #[test]
    fn a_second_window_on_a_resident_image_needs_no_decode() {
        let (mut store, _held) = resident_full();
        // A second window requesting the same key shares the resident texture with
        // no new work — the keyed entry is the cross-window dedup.
        let (lease, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        assert!(out.jobs.is_empty());
        assert!(lease.texture().is_some());
    }
}
