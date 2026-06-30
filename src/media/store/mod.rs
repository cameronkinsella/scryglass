//! The single texture-lifecycle store: the one owner of every still image's
//! decoded RAM and GPU texture, keyed by image identity and shared across all
//! windows. Three invariants are structural rather than checked by hand.
//!
//! One texture per image: identity is [`ImageKey`] (container + entry), the
//! store is the only minter, and holders get `Arc` clones of the one texture.
//! No invalid tier move: the typestate tier values expose only forward-release
//! transitions, each consuming `self`, so a backward move does not compile.
//! Demote/evict only once every holder permits: atomic per-tier counters
//! ([`TierCounters`]) shared with every lease keep the image at `max_wanted()`,
//! recomputed O(1) on each lease event.

use std::cell::Cell as StdCell;
use std::collections::HashMap;
use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use iced::widget::image::Handle;

use crate::media::animation::AnimatedImage;
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

/// A kind of resident media the [`Store`] can own, dedup across windows, and decay.
///
/// The store provides the machinery generically: cross-window dedup by [`ImageKey`],
/// refcounted demand, decode-on-demand, and free-when-unwanted. A medium provides
/// only the resident data and the per-tier transitions that machinery drives. Each
/// medium declares the highest tier it uses; the store clamps to it, so a tier a
/// medium never reaches is statically unreachable rather than a runtime special case.
pub trait Medium {
    /// The resident state at rest, including its tier (`Default` is the evicted state).
    type State: Default;
    /// The shared resource a holder reads from the cell each frame to display it.
    type Shared;
    /// The decoded RAM form a decode job produces.
    type Ram: Clone;
    /// The GPU resource an upload job produces; an uninhabited type for a medium that
    /// never uploads (it then has no tier above `InRam`, so a mint cannot occur).
    type Minted;

    /// The highest tier this medium uses. A request above it is clamped down.
    const MAX_TIER: Tier;

    /// The tier of a resident state.
    fn tier(state: &Self::State) -> Tier;
    /// The resource a holder displays, if the state currently holds one.
    fn shared(state: &Self::State) -> Option<Arc<Self::Shared>>;
    /// The RAM source, if resident above eviction (a clone is a refcount bump).
    fn ram(state: &Self::State) -> Option<Self::Ram>;
    /// Free a state down to `target`, never producing a tier above its input.
    fn demote(state: Self::State, target: Tier) -> Self::State;
    /// The state a fresh decode lands in: RAM resident, no GPU resource yet.
    fn from_ram(ram: Self::Ram) -> Self::State;
    /// Install an uploaded resource at `tier`, returning the new state. Unreachable
    /// when `Minted` is uninhabited.
    fn mint(state: Self::State, tier: Tier, minted: Self::Minted) -> Self::State;
    /// The measured decode time of the resident RAM, for the dynamic evict delay.
    fn decode_time(state: &Self::State) -> Option<Duration>;
}

// --- typestate tier values: forward-release transitions consume self. The
// invalid (backward) moves have no method, so they cannot be written. ---

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
#[derive(Default)]
pub enum CellState {
    #[default]
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

/// The still-image medium: a single decoded raster uploaded to a tiered GPU texture.
/// It uses all four tiers (`Evicted → InRam → View → Full`); demote drops the texture
/// back to RAM, evict frees the RAM. Its decode time feeds the dynamic evict delay.
pub struct Still;

impl Medium for Still {
    type State = CellState;
    type Shared = ResidentImage;
    type Ram = RamImage;
    type Minted = Keepalive;

    const MAX_TIER: Tier = Tier::Full;

    fn tier(state: &CellState) -> Tier {
        state.tier()
    }

    fn shared(state: &CellState) -> Option<Keepalive> {
        state.texture()
    }

    fn ram(state: &CellState) -> Option<RamImage> {
        state.ram()
    }

    fn demote(state: CellState, target: Tier) -> CellState {
        demote_to(state, target)
    }

    fn from_ram(ram: RamImage) -> CellState {
        CellState::InRam(ram)
    }

    fn mint(state: CellState, tier: Tier, texture: Keepalive) -> CellState {
        // The RAM must still be resident to upload from; if it was evicted between
        // the decode and this mint, keep the state as-is and a later request re-decodes.
        match state.ram() {
            Some(ram) if tier == Tier::Full => CellState::Full(FullTexture { ram, texture }),
            Some(ram) => CellState::View(ViewTexture { ram, texture }),
            None => state,
        }
    }

    fn decode_time(state: &CellState) -> Option<Duration> {
        match state {
            CellState::Full(f) => f.ram.decode_time,
            CellState::View(v) => v.ram.decode_time,
            CellState::InRam(r) => r.decode_time,
            CellState::Evicted => None,
        }
    }
}

/// The decoded frames of an animation in RAM, plus how long the decode took (for
/// the dynamic evict delay). A clone is a refcount bump on the shared frames, so a
/// second window displays the same decode with no work of its own.
#[derive(Clone)]
pub struct AnimRam {
    pub frames: Arc<AnimatedImage>,
    pub decode_time: Option<Duration>,
}

/// An animation's resident state at rest. Unlike a still it has no GPU tier: every
/// window composites and uploads its own frames at its own rate, so the store owns
/// only the shared decoded RAM, which is present (`InRam`) or evicted.
#[derive(Default)]
pub enum AnimState {
    #[default]
    Evicted,
    InRam(AnimRam),
}

/// The animated-image medium: decoded frames shared in RAM across windows and
/// evicted as a unit, with no GPU tier. It lives in the `Evicted`/`InRam` band, so
/// `Minted = Infallible` makes an upload (and any tier above `InRam`) unrepresentable.
pub struct Anim;

impl Medium for Anim {
    type State = AnimState;
    type Shared = AnimatedImage;
    type Ram = AnimRam;
    type Minted = Infallible;

    const MAX_TIER: Tier = Tier::InRam;

    fn tier(state: &AnimState) -> Tier {
        match state {
            AnimState::Evicted => Tier::Evicted,
            AnimState::InRam(_) => Tier::InRam,
        }
    }

    fn shared(state: &AnimState) -> Option<Arc<AnimatedImage>> {
        match state {
            AnimState::InRam(r) => Some(r.frames.clone()),
            AnimState::Evicted => None,
        }
    }

    fn ram(state: &AnimState) -> Option<AnimRam> {
        match state {
            AnimState::InRam(r) => Some(r.clone()),
            AnimState::Evicted => None,
        }
    }

    fn demote(state: AnimState, target: Tier) -> AnimState {
        // The only tier below InRam is Evicted, so a demote either keeps the frames
        // or frees them. There is no GPU tier in between to drop to.
        if target < Tier::InRam {
            AnimState::Evicted
        } else {
            state
        }
    }

    fn from_ram(ram: AnimRam) -> AnimState {
        AnimState::InRam(ram)
    }

    fn mint(_state: AnimState, _tier: Tier, minted: Infallible) -> AnimState {
        // An animation never uploads through the store, so the store never mints
        // it: `Infallible` is uninhabited, so this match has no arms to write.
        match minted {}
    }

    fn decode_time(state: &AnimState) -> Option<Duration> {
        match state {
            AnimState::InRam(r) => r.decode_time,
            AnimState::Evicted => None,
        }
    }
}

/// One swappable texture slot per image, shared (`Arc`) by every holder. The store
/// swaps it once per tier change (O(1), with no fan-out over holders), and every
/// holder reads it at render time. Empty while the image is still decoding, so the
/// display falls back to its thumbnail blur (never a black frame).
pub struct Cell<T> {
    slot: arc_swap::ArcSwapOption<T>,
}

impl<T> Default for Cell<T> {
    fn default() -> Self {
        Self {
            slot: arc_swap::ArcSwapOption::default(),
        }
    }
}

impl<T> Cell<T> {
    /// The current shared resource, or `None` while pending / not resident.
    pub fn load(&self) -> Option<Arc<T>> {
        self.slot.load_full()
    }

    /// Install (or clear) the shared resource. The previous one frees once its last
    /// clone drops.
    pub fn store(&self, value: Option<Arc<T>>) {
        self.slot.store(value);
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
pub enum Job<M: Medium = Still> {
    /// Decode `path` from `source` to RAM, then call [`Store::on_decoded`].
    Decode {
        key: ImageKey,
        path: PathBuf,
        source: Source,
    },
    /// Upload the held `ram` for `key` at `tier`, then call [`Store::on_minted`].
    /// `source` is the currently resident resource, if any: a full -> view demote
    /// lets the app downscale that texture on the GPU through the display shader
    /// instead of resizing the RAM on the CPU, keeping the swap seamless.
    Upload {
        key: ImageKey,
        tier: Tier,
        ram: M::Ram,
        source: Option<Arc<M::Shared>>,
    },
}

pub struct StoreOutcome<M: Medium = Still> {
    pub jobs: Vec<Job<M>>,
}

impl<M: Medium> Default for StoreOutcome<M> {
    fn default() -> Self {
        Self { jobs: Vec::new() }
    }
}

/// Keys whose demand changed via a `Lease` drop, drained by [`Store::pump`]. A drop
/// can't run store logic (no `&mut Store`, can't be async), so it records the key
/// here and the next pump reconciles it. O(keys dirtied), never a scan.
type Dirty = Arc<Mutex<Vec<ImageKey>>>;

/// A holder's claim on an image at a tier. Held in window state (a display slot or a
/// prefetch slot). Dropping it lowers the demand by one, RAII, with no store call.
pub struct Lease<M: Medium = Still> {
    key: ImageKey,
    want: StdCell<Tier>,
    counters: Arc<TierCounters>,
    cell: Arc<Cell<M::Shared>>,
    dirty: Dirty,
}

impl<M: Medium> Lease<M> {
    /// The shared resource to read at render time (or `None` while pending).
    pub fn texture(&self) -> Option<Arc<M::Shared>> {
        self.cell.load()
    }

    /// This holder's current demand.
    pub fn want(&self) -> Tier {
        self.want.get()
    }
}

impl<M: Medium> Drop for Lease<M> {
    fn drop(&mut self) {
        self.counters.dec(self.want.get());
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.push(self.key.clone());
        }
    }
}

struct Entry<M: Medium> {
    counters: Arc<TierCounters>,
    /// Holders own the strong `Arc<Cell<_>>` and the entry only borrows it, so a
    /// dead weak means no holder is leasing this image right now.
    cell: Weak<Cell<M::Shared>>,
    state: M::State,
    /// How to re-decode this image, kept so any window's request can drive it.
    path: PathBuf,
    source: Source,
    /// The tier a decode/upload is currently in flight for, so a reconcile does not
    /// fire a duplicate job while one is pending.
    pending: Option<Tier>,
}

impl<M: Medium> Entry<M> {
    fn is_leased(&self) -> bool {
        self.cell.strong_count() > 0
    }
}

/// The single resource-lifecycle store: the one owner of every resident image's tier,
/// keyed by [`ImageKey`]. Holders talk to it through [`Lease`]s, and it answers with
/// the async [`Job`]s the app should run. All decisions are O(1), a hash lookup and
/// the fixed four-slot `max_wanted`. Nothing iterates windows or holders.
pub struct Store<M: Medium = Still> {
    entries: HashMap<ImageKey, Entry<M>>,
    dirty: Dirty,
}

impl<M: Medium> Default for Store<M> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            dirty: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<M: Medium> Store<M> {
    /// A holder claims `key` at `want`. Returns a [`Lease`] (its `texture()` is
    /// `None` until the tier is resident) and any work to start. O(1). A `want`
    /// above the medium's `MAX_TIER` is clamped down to it.
    pub fn request(
        &mut self,
        key: ImageKey,
        path: PathBuf,
        source: Source,
        want: Tier,
    ) -> (Lease<M>, StoreOutcome<M>) {
        let want = want.min(M::MAX_TIER);
        let dirty = self.dirty.clone();
        let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
            counters: Arc::new(TierCounters::default()),
            cell: Weak::new(),
            state: M::State::default(),
            path,
            source,
            pending: None,
        });
        entry.counters.inc(want);
        // Reuse the live shared cell, or seed a fresh one with whatever is resident.
        let cell = entry.cell.upgrade().unwrap_or_else(|| {
            let cell = Arc::new(Cell::default());
            cell.store(M::shared(&entry.state));
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

    /// Lower (or raise) a holder's demand in place, e.g. a decay step. O(1). A `to`
    /// above the medium's `MAX_TIER` is clamped down to it.
    pub fn retarget(&mut self, lease: &Lease<M>, to: Tier) -> StoreOutcome<M> {
        let to = to.min(M::MAX_TIER);
        lease.counters.dec(lease.want.get());
        lease.counters.inc(to);
        lease.want.set(to);
        self.reconcile(&lease.key)
    }

    /// A decode finished: the image is now in RAM. Drive it on toward demand.
    pub fn on_decoded(&mut self, key: ImageKey, ram: M::Ram) -> StoreOutcome<M> {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pending = None;
            if M::tier(&entry.state) == Tier::Evicted {
                entry.state = M::from_ram(ram);
                // Refresh the shared cell: a medium whose resident RAM is itself the
                // displayed resource (no upload tier) becomes visible right here. For
                // a medium that displays an uploaded texture this is `None` until the
                // upload lands, so the store is a no-op for it.
                if let Some(cell) = entry.cell.upgrade() {
                    cell.store(M::shared(&entry.state));
                }
            }
        }
        self.reconcile(&key)
    }

    /// A decode never produced a still: it was cancelled by a newer navigation
    /// (`retry` = keep the entry and re-emit a decode if still wanted) or it
    /// failed / turned out to be an animation (`retry` false = forget it). Clears
    /// the in-flight marker either way so the store is not stuck pending.
    pub fn on_decode_failed(&mut self, key: &ImageKey, retry: bool) -> StoreOutcome<M> {
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

    /// An upload finished: install the resource at `tier` and swap the shared cell.
    pub fn on_minted(&mut self, key: ImageKey, tier: Tier, minted: M::Minted) -> StoreOutcome<M> {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.pending = None;
            entry.state = M::mint(std::mem::take(&mut entry.state), tier, minted);
            if let Some(cell) = entry.cell.upgrade() {
                cell.store(M::shared(&entry.state));
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
    pub fn ram(&self, key: &ImageKey) -> Option<M::Ram> {
        self.entries.get(key).and_then(|e| M::ram(&e.state))
    }

    /// The image's shared resident resource (its texture, or a tiled still's
    /// pyramid), if any tier currently holds one.
    pub fn shared(&self, key: &ImageKey) -> Option<Arc<M::Shared>> {
        self.entries.get(key).and_then(|e| M::shared(&e.state))
    }

    /// The image's current resident tier, or `Evicted` if the store is not
    /// tracking it.
    pub fn tier(&self, key: &ImageKey) -> Tier {
        self.entries
            .get(key)
            .map_or(Tier::Evicted, |e| M::tier(&e.state))
    }

    /// The measured read + decode time for this image, feeding the dynamic
    /// eviction delay. `None` if the image is not resident or was never timed
    /// (e.g. reused from another window's decode).
    pub fn decode_time(&self, key: &ImageKey) -> Option<Duration> {
        let entry = self.entries.get(key)?;
        M::decode_time(&entry.state)
    }

    /// An upload could not reach the GPU (the upload thread was not ready). Clear
    /// the in-flight marker and reconcile, so the next pass re-attempts the mint
    /// if the tier is still wanted.
    pub fn on_mint_failed(&mut self, key: &ImageKey) -> StoreOutcome<M> {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.pending = None;
        }
        self.reconcile(key)
    }

    /// Reconcile every image whose demand changed via a dropped lease. O(keys dirty).
    pub fn pump(&mut self) -> StoreOutcome<M> {
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
    fn reconcile(&mut self, key: &ImageKey) -> StoreOutcome<M> {
        let Some(entry) = self.entries.get_mut(key) else {
            return StoreOutcome::default();
        };
        let want = entry.counters.max_wanted();
        let have = M::tier(&entry.state);

        if want == have {
            // A decode cancelled after its last lease dropped leaves an unleased
            // evicted entry that the demote path below never sees. Reap it here.
            if have == Tier::Evicted && entry.pending.is_none() && !entry.is_leased() {
                self.entries.remove(key);
            }
            return StoreOutcome::default();
        }

        if want < have {
            // Freeing toward a lower tier is synchronous, EXCEPT demoting a full
            // texture to view, which is a re-upload at a smaller size (handled as an
            // acquire below). Full/View -> InRam/Evicted drops the GPU/RAM.
            if want == Tier::View && have == Tier::Full {
                return self.acquire(key, Tier::View);
            }
            let state = std::mem::take(&mut entry.state);
            entry.state = M::demote(state, want);
            if let Some(cell) = entry.cell.upgrade() {
                cell.store(M::shared(&entry.state));
            }
            // A fully-evicted, unleased image is forgotten (a later request re-decodes).
            if M::tier(&entry.state) == Tier::Evicted && !entry.is_leased() {
                self.entries.remove(key);
            }
            return StoreOutcome::default();
        }

        // want > have: acquire the wanted tier.
        self.acquire(key, want)
    }

    /// Emit the single job to move an image up to `target` (decode if evicted, else
    /// upload from the RAM it already holds), unless one is already in flight.
    fn acquire(&mut self, key: &ImageKey, target: Tier) -> StoreOutcome<M> {
        let Some(entry) = self.entries.get_mut(key) else {
            return StoreOutcome::default();
        };
        // One job per image at a time. A decode fills RAM whatever tier asked for
        // it, and every completion reconciles again, so demand that rose mid-flight
        // is picked up by the completion instead of a duplicate job here.
        if entry.pending.is_some() {
            return StoreOutcome::default();
        }
        let job = match M::ram(&entry.state) {
            None => Job::Decode {
                key: key.clone(),
                path: entry.path.clone(),
                source: entry.source.clone(),
            },
            // Carry the currently resident resource so a full -> view demote can
            // downscale that texture on the GPU rather than resizing RAM on the CPU.
            Some(ram) => Job::Upload {
                key: key.clone(),
                tier: target,
                ram,
                source: M::shared(&entry.state),
            },
        };
        entry.pending = Some(target);
        StoreOutcome { jobs: vec![job] }
    }
}

/// Free an owned tier value down to `target` through the synchronous typestate
/// transitions (full/view drop their texture to RAM, and RAM evicts). It never
/// produces a tier above its input, so the pipeline only ever runs forward.
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

        // The focused window leaves: it falls to the next demand, not below it,
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
        let cell = Cell::<ResidentImage>::default();
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
        let mut store: Store = Store::default();
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
        // request has to decode again, proving the texture was actually released.
        drop(b);
        let _ = store.pump();
        let (_c, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
    }

    #[test]
    fn lowering_demand_to_view_re_uploads_at_view_resolution() {
        let (mut store, lease) = resident_full();
        // The only holder decays Full -> View: the store re-uploads a smaller
        // texture rather than dropping (a view tier is a downscale, not a free).
        // The demote carries the resident full texture as `source`, so the app can
        // downscale it on the GPU instead of resizing RAM.
        let out = store.retarget(&lease, Tier::View);
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::View,
                source: Some(_),
                ..
            }]
        ));
    }

    #[test]
    fn a_fresh_view_upload_carries_no_source_texture() {
        // A first-time View request has no resident texture to render from, so its
        // upload leaves `source` empty and the app falls back to a CPU downscale.
        let mut store: Store = Store::default();
        let (_lease, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::View);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
        let out = store.on_decoded(key(), ram());
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::View,
                source: None,
                ..
            }]
        ));
    }

    #[test]
    fn a_second_window_on_a_resident_image_needs_no_decode() {
        let (mut store, _held) = resident_full();
        // A second window requesting the same key shares the resident texture with
        // no new work, the keyed entry is the cross-window dedup.
        let (lease, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);
        assert!(out.jobs.is_empty());
        assert!(lease.texture().is_some());
    }

    // --- The Anim medium runs through the very same machinery as Still, proving an
    // animation's decoded RAM and decay are shared and refcounted across windows. ---

    fn anim_key() -> ImageKey {
        ImageKey::new(&Source::Fs, Path::new("a.gif"))
    }

    fn anim_ram() -> AnimRam {
        AnimRam {
            frames: Arc::new(AnimatedImage {
                width: 2,
                height: 2,
                frames: Vec::new(),
                thumbnail: None,
            }),
            decode_time: None,
        }
    }

    #[test]
    fn an_animation_decodes_into_shared_ram_then_evicts() {
        let mut store: Store<Anim> = Store::default();
        let (lease, out) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
        assert!(lease.texture().is_none()); // decoding -> the view shows its thumb

        // InRam is the medium's top tier, so the decode resolves the demand outright:
        // no upload follows, and the shared frames are immediately what the view reads.
        let out = store.on_decoded(anim_key(), anim_ram());
        assert!(out.jobs.is_empty());
        assert!(lease.texture().is_some());

        // A decay evict frees the frames while the holder lives on, so the view falls
        // back to its thumbnail, the same shape a still takes when its cell empties.
        let out = store.retarget(&lease, Tier::Evicted);
        assert!(out.jobs.is_empty());
        assert!(lease.texture().is_none());
    }

    #[test]
    fn an_animation_request_is_clamped_below_the_gpu_tiers() {
        // A holder asking for Full is clamped to the medium's InRam ceiling: the store
        // decodes to RAM and stops, never emitting an upload (it has no GPU tier).
        let mut store: Store<Anim> = Store::default();
        let (lease, _) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::Full);
        let out = store.on_decoded(anim_key(), anim_ram());
        assert!(out.jobs.is_empty());
        assert_eq!(store.tier(&anim_key()), Tier::InRam);
        assert!(lease.texture().is_some());
    }

    #[test]
    fn a_second_window_shares_one_animation_decode() {
        let mut store: Store<Anim> = Store::default();
        let (a, _) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);
        store.on_decoded(anim_key(), anim_ram());

        // The second window reuses the decode with no new work and reads the very same
        // frames in RAM: one decode, shared by pointer, not copied per window.
        let (b, out) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);
        assert!(out.jobs.is_empty());
        let frames_a = a.texture().unwrap();
        let frames_b = b.texture().unwrap();
        assert!(Arc::ptr_eq(&frames_a, &frames_b));
    }

    #[test]
    fn dropping_every_holder_frees_the_animation() {
        let mut store: Store<Anim> = Store::default();
        let (a, _) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);
        store.on_decoded(anim_key(), anim_ram());
        assert!(store.ram(&anim_key()).is_some());

        // The last holder lets go: nobody wants the frames, so they are evicted and a
        // fresh request has to decode again (RAII, the same as a still).
        drop(a);
        let _ = store.pump();
        let (_b, out) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));
    }

    #[test]
    fn a_redundant_decode_is_discarded_not_duplicated() {
        let mut store: Store<Anim> = Store::default();
        let (lease, _) = store.request(anim_key(), "a.gif".into(), Source::Fs, Tier::InRam);

        // The first decode lands and becomes the one resident allocation.
        let first = anim_ram();
        let first_frames = first.frames.clone();
        store.on_decoded(anim_key(), first);

        // A second decode of the same image produces a genuinely distinct allocation
        // (a re-decode, or a concurrent first-open race). It must not be installed.
        let second = anim_ram();
        let second_frames = second.frames.clone();
        assert!(!Arc::ptr_eq(&first_frames, &second_frames));
        store.on_decoded(anim_key(), second);

        // The store kept the original and threw the duplicate away: the resident frames
        // and the holder's lease both still point at the first allocation, not a copy.
        let resident = store.ram(&anim_key()).unwrap().frames;
        assert!(Arc::ptr_eq(&resident, &first_frames));
        assert!(!Arc::ptr_eq(&resident, &second_frames));
        assert!(Arc::ptr_eq(&lease.texture().unwrap(), &first_frames));
    }

    #[test]
    fn escalating_a_pending_decode_fires_no_second_decode() {
        let mut store: Store = Store::default();
        let (lease, out) = store.request(key(), "a.png".into(), Source::Fs, Tier::View);
        assert!(matches!(out.jobs.as_slice(), [Job::Decode { .. }]));

        // Stepping onto a prefetched neighbor raises demand mid-decode. The
        // in-flight decode already yields full RAM, so no second read starts.
        let out = store.retarget(&lease, Tier::Full);
        assert!(out.jobs.is_empty());

        // The one decode lands and its completion drives straight to the
        // escalated tier.
        let out = store.on_decoded(key(), ram());
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::Full,
                ..
            }]
        ));
    }

    #[test]
    fn escalating_a_pending_upload_defers_to_its_completion() {
        let mut store: Store = Store::default();
        let (lease, _) = store.request(key(), "a.png".into(), Source::Fs, Tier::View);
        let out = store.on_decoded(key(), ram());
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::View,
                ..
            }]
        ));

        // Demand rises while the view upload is in flight: nothing new fires.
        let out = store.retarget(&lease, Tier::Full);
        assert!(out.jobs.is_empty());

        // The view mint lands, and its reconcile emits the one full upload.
        let out = store.on_minted(key(), Tier::View, keep());
        assert!(matches!(
            out.jobs.as_slice(),
            [Job::Upload {
                tier: Tier::Full,
                ..
            }]
        ));
    }

    #[test]
    fn a_cancelled_decode_reaps_the_unleased_entry() {
        let mut store: Store = Store::default();
        let (lease, _) = store.request(key(), "a.png".into(), Source::Fs, Tier::Full);

        // Navigation sheds the lease and cancels the decode before it lands.
        drop(lease);
        let _ = store.pump();
        let out = store.on_decode_failed(&key(), true);
        assert!(out.jobs.is_empty());
        assert!(store.entries.is_empty());
    }
}
