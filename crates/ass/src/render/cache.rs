//! What the renderer keeps between frames within a limit: outlines by
//! count, glyph bitmaps and composited runs by bytes, as libass 0.17.5
//! keeps them (`ass_cache.c`).
//!
//! libass queues a cache's entries by their last use (`ass_cache_get`,
//! `ass_cache.c:384-445`). Cutting it to a size (`ass_cache_cut`,
//! `:487-515`), which it does as each frame starts, pops that queue from the
//! oldest: an entry nothing else holds goes, and one still held, by the
//! previous frame's images or by another entry's key, leaves the queue but
//! stays in the cache, findable and counted, until its last holder lets go
//! (`ass_cache_dec_ref`, `:469-485`) or a lookup queues it again. The cut
//! stops once the cache counts at most the limit. Whatever the previous
//! frame's images show therefore survives the cut, so an equal key finds the
//! same value in the next frame, and a value is identified by itself.
//!
//! The renderer's keys hold what libass's do, but for a composite's: it
//! holds its glyphs' bitmap keys, and through them their outlines, where
//! libass's holds the bitmaps. Equal bitmap keys draw equal bitmaps, so it
//! finds the same composites, and the bitmaps a shown composite was made
//! from can leave the cache.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher, RandomState};
use std::ops::Deref;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use triergon_core::{Budget, BudgetVec, Error, Reservation, Result};

/// What libass counts for an entry beyond its value: its item record
/// (`CACHE_ITEM_SIZE`), six pointers and two sizes.
const ITEM_BYTES: usize = sizes::item(size_of::<usize>());

/// No entry: the end of the use queue or of a hash chain.
const NONE: u32 = u32::MAX;

/// A value a cache keeps, shared with whatever holds it; the entry is held
/// while anything besides the cache does.
#[derive(Debug)]
pub(super) struct Kept<T>(Arc<Node<T>>);

impl<T> Kept<T> {
    /// Whether `a` and `b` are the same kept value.
    pub(super) fn ptr_eq(a: &Self, b: &Self) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    /// The kept value's address, which identifies it while it is kept.
    pub(super) fn as_ptr(this: &Self) -> *const () {
        Arc::as_ptr(&this.0).cast()
    }
}

impl<T> Clone for Kept<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Deref for Kept<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0.value
    }
}

/// A kept value and where to tell its cache when its last holder lets go
/// of it.
#[derive(Debug)]
struct Node<T> {
    value: T,
    released: Released,
}

/// The entries whose values were let go since the cache last looked, by
/// slot and generation.
type ReleaseLog = Arc<Mutex<Vec<(u32, u32)>>>;

#[derive(Debug)]
struct Released {
    log: ReleaseLog,
    slot: u32,
    generation: u32,
}

impl<T> Drop for Node<T> {
    fn drop(&mut self) {
        let Released {
            log,
            slot,
            generation,
        } = &self.released;
        // The cache keeps room for a record per entry, so this does not
        // allocate.
        lock(log).push((*slot, *generation));
    }
}

/// The release log, locked. A poisoned log only means a holder panicked
/// while pushing, which leaves the records whole.
fn lock(log: &ReleaseLog) -> std::sync::MutexGuard<'_, Vec<(u32, u32)>> {
    log.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A key of a cache, looked up through a borrowed view: a drawing by the
/// text the script holds, a composite by the key being made. A key and its
/// view must hash and compare alike.
pub(super) trait CacheKey {
    type View<'a>: Hash
    where
        Self: 'a;

    fn view(&self) -> Self::View<'_>;

    /// Whether two views, borrowed for different lifetimes, are equal.
    fn same(a: &Self::View<'_>, b: &Self::View<'_>) -> bool;
}

/// What a lookup finds.
#[derive(Debug)]
pub(super) enum Lookup<T> {
    /// Nothing is kept for the key.
    Miss,
    /// The key is kept without a value: an outline that cannot be built.
    Empty,
    /// The value kept.
    Hit(Kept<T>),
}

/// An entry's value: kept by the cache, or, cut while held, kept only by its
/// holders.
#[derive(Debug)]
enum Value<T> {
    Empty,
    Kept(Arc<Node<T>>),
    Lent(Weak<Node<T>>),
}

#[derive(Debug)]
struct Entry<K, T> {
    key: K,
    hash: u64,
    /// The next entry of the same hash.
    chain: u32,
    value: Value<T>,
    /// What libass counts for it.
    size: usize,
    /// Neighbours in the use queue, older and newer; an entry cut while held
    /// is in no queue.
    older: u32,
    newer: u32,
}

#[derive(Debug)]
enum Slot<K, T> {
    Free { next: u32, generation: u32 },
    Used { entry: Entry<K, T>, generation: u32 },
}

/// The hasher of a map whose keys are hashes already.
#[derive(Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

/// Values of `T` by keys `K`, cut to a limit as libass cuts its caches, and
/// charged to the renderer's budget: its slots, its index, each value's
/// shared allocation and the room its release records need. What a key or
/// value holds beyond them is charged where it is allocated.
#[derive(Debug)]
pub(super) struct Cache<K, T> {
    slots: BudgetVec<Slot<K, T>>,
    /// The first free slot.
    free: u32,
    /// The first entry of each hash.
    index: HashMap<u64, u32, BuildHasherDefault<Prehashed>>,
    hasher: RandomState,
    /// The use queue, oldest and newest.
    oldest: u32,
    newest: u32,
    released: ReleaseLog,
    len: usize,
    /// The entries with a value, whose shared allocations live until the
    /// entries go.
    values: usize,
    /// libass's `cache_size`: what the entries kept count, held ones too.
    size: usize,
    limit: usize,
    /// The index's, the values' and the release log's storage.
    charge: Reservation,
}

impl<K: CacheKey, T> Cache<K, T> {
    /// The bytes an index bucket takes: a hash, a slot and a control byte.
    const BUCKET_BYTES: usize = size_of::<(u64, u32)>() + 1;

    /// The control bytes an index adds past its buckets, at most: a group
    /// of the widest probe.
    const INDEX_TAIL_BYTES: usize = 16;

    /// A value's shared allocation: the node and its two reference counts.
    const NODE_BYTES: usize = size_of::<Node<T>>() + 2 * size_of::<usize>();

    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            slots: BudgetVec::new(budget),
            free: NONE,
            index: HashMap::default(),
            hasher: RandomState::new(),
            oldest: NONE,
            newest: NONE,
            released: Arc::default(),
            len: 0,
            values: 0,
            size: 0,
            limit: usize::MAX,
            charge: Reservation::empty(budget),
        }
    }

    /// The value kept for `key`, which is now the newest in the use queue,
    /// queued again if it was cut while held (`ass_cache_get`).
    pub(super) fn get(&mut self, key: K::View<'_>) -> Lookup<T> {
        let Some(at) = self.find(&key) else {
            return Lookup::Miss;
        };
        let entry = self.entry_mut(at);
        let found = match &entry.value {
            Value::Empty => Lookup::Empty,
            Value::Kept(node) => Lookup::Hit(Kept(Arc::clone(node))),
            Value::Lent(node) => {
                let Some(node) = node.upgrade() else {
                    // Let go but not yet taken out: as good as gone.
                    self.remove(at);
                    return Lookup::Miss;
                };
                entry.value = Value::Kept(Arc::clone(&node));
                Lookup::Hit(Kept(node))
            }
        };
        self.unqueue(at);
        self.queue(at);
        found
    }

    /// Keeps `value` for `key`, absent before, counted as `size` and,
    /// unless it counts 1 as an outline does, its item record
    /// (`ass_cache.c:440`); then cuts the cache to its limit if it is past
    /// it.
    ///
    /// # Errors
    ///
    /// `Overflow` when the count would, and the budget's errors; the cache is
    /// then unchanged.
    pub(super) fn insert(&mut self, key: K, value: T, size: usize) -> Result<Kept<T>> {
        let at = self.add(key, Some(value), size)?;
        let Value::Kept(node) = &self.entry(at).value else {
            unreachable!("a value just kept");
        };
        let kept = Kept(Arc::clone(node));
        self.shrink();
        Ok(kept)
    }

    /// Keeps `key` without a value, counted as [`insert`](Self::insert)
    /// counts one.
    ///
    /// # Errors
    ///
    /// Those of [`insert`](Self::insert).
    pub(super) fn insert_empty(&mut self, key: K, size: usize) -> Result<()> {
        self.add(key, None, size)?;
        self.shrink();
        Ok(())
    }

    /// Adds an entry for `key` as the newest in the queue, without cutting;
    /// its slot.
    fn add(&mut self, key: K, value: Option<T>, size: usize) -> Result<u32> {
        let hash = self.hash(&key.view());
        debug_assert!(self.find(&key.view()).is_none(), "a key kept twice");
        let size = if size == 1 {
            Some(1)
        } else {
            size.checked_add(ITEM_BYTES)
        };
        let total = size
            .and_then(|size| self.size.checked_add(size).map(|total| (size, total)))
            .ok_or(Error::Overflow)?;
        self.reserve()?;
        let (size, total) = total;
        let (at, generation) = self.take_slot();
        let value = match value {
            Some(value) => {
                self.values += 1;
                Value::Kept(Arc::new(Node {
                    value,
                    released: Released {
                        log: Arc::clone(&self.released),
                        slot: at,
                        generation,
                    },
                }))
            }
            None => Value::Empty,
        };
        let chain = self.index.insert(hash, at).unwrap_or(NONE);
        let entry = Entry {
            key,
            hash,
            chain,
            value,
            size,
            older: NONE,
            newer: NONE,
        };
        self.slots[at as usize] = Slot::Used { entry, generation };
        self.len += 1;
        self.size = total;
        self.queue(at);
        Ok(at)
    }

    /// Cuts the cache to `limit` as `ass_cache_cut` does, and keeps it
    /// within `limit` until the next cut.
    ///
    /// libass cuts only when a frame starts, so whatever a frame draws stays
    /// until the next one. This cache is also cut, the same way, when an
    /// insertion takes it past `limit` and when the renderer lets go of a run
    /// or an event ([`settle`](Self::settle)), so it holds at most `limit`
    /// beyond what is in use. Those cuts never take an entry something holds,
    /// so nothing a frame shows goes, and an entry looked up again is drawn
    /// the same under its key: images and change values stay libass's.
    pub(super) fn cut(&mut self, limit: usize) {
        self.limit = limit;
        self.shrink();
    }

    /// Cuts the cache to its limit again once the renderer has let go of
    /// what it drew.
    pub(super) fn settle(&mut self) {
        self.shrink();
    }

    /// Takes out the entries let go of since the last look, as libass
    /// frees an entry cut while held when its last holder lets go; then,
    /// oldest first, an entry nothing holds goes and a held one leaves the
    /// queue, until the cache counts at most its limit. Taking an entry out
    /// lets go of its key, which may let go of another entry, as libass's
    /// keys hold the entries they name.
    fn shrink(&mut self) {
        self.take_released();
        while self.size > self.limit && self.oldest != NONE {
            let at = self.oldest;
            self.unqueue(at);
            let entry = self.entry_mut(at);
            let held = match &entry.value {
                Value::Kept(node) if Arc::strong_count(node) > 1 => Some(Arc::downgrade(node)),
                _ => None,
            };
            match held {
                Some(node) => entry.value = Value::Lent(node),
                None => self.remove(at),
            }
            self.take_released();
        }
        if self.index.capacity() > 4 * self.index.len() + 64 {
            self.index.shrink_to(2 * self.index.len());
        }
        // Shrinking a charge does not fail.
        let _ = self.charge.resize_to(self.charged_bytes());
    }

    /// Takes out the entries whose values were let go, cut while held.
    fn take_released(&mut self) {
        loop {
            let record = lock(&self.released).pop();
            let Some((at, generation)) = record else {
                break;
            };
            if let Some(Slot::Used {
                entry,
                generation: current,
            }) = self.slots.get(at as usize)
                && *current == generation
                && matches!(entry.value, Value::Lent(_))
            {
                self.remove(at);
            }
        }
    }

    /// Room for one more entry: a slot, its index bucket, its value and its
    /// release record, charged before anything changes.
    ///
    /// # Errors
    ///
    /// `Overflow` when every slot number is taken, and the budget's errors.
    fn reserve(&mut self) -> Result<()> {
        if self.free == NONE {
            // A new slot's number must not be `NONE`.
            if u32::try_from(self.slots.len()).map_or(true, |len| len == NONE) {
                return Err(Error::Overflow);
            }
            if self.slots.len() == self.slots.capacity() {
                // By half: a run holds its entries, so the slab grows to the
                // longest run, and keeps the slack.
                self.slots
                    .try_reserve_exact((self.slots.len() / 2).max(16))?;
            }
        }
        self.index.try_reserve(1).map_err(|_| Error::Allocation)?;
        let records = self.slots.capacity() + 1;
        let mut log = lock(&self.released);
        if log.capacity() < records {
            let len = log.len();
            log.try_reserve_exact(records - len)
                .map_err(|_| Error::Allocation)?;
        }
        drop(log);
        // One value more than kept.
        let bytes = self.charged_bytes().saturating_add(Self::NODE_BYTES);
        self.charge.resize_to(bytes)
    }

    /// What the cache charges beyond its slots: its index, by buckets as
    /// the map lays them out; its values' shared allocations; and its
    /// release log.
    fn charged_bytes(&self) -> usize {
        let log = lock(&self.released)
            .capacity()
            .saturating_mul(size_of::<(u32, u32)>());
        // The map's buckets are a power of two, all but an eighth of them
        // usable past eight, all but one below.
        let capacity = self.index.capacity();
        let buckets = match capacity {
            0 => 0,
            1..=3 => 4,
            4..=7 => 8,
            _ => capacity / 7 * 8,
        };
        let index = if buckets == 0 {
            0
        } else {
            buckets
                .saturating_mul(Self::BUCKET_BYTES)
                .saturating_add(Self::INDEX_TAIL_BYTES)
        };
        let values = self.values.saturating_mul(Self::NODE_BYTES);
        index.saturating_add(values).saturating_add(log)
    }

    /// A free slot and its generation, from the free list or new.
    fn take_slot(&mut self) -> (u32, u32) {
        if let Some(Slot::Free { next, generation }) = self.slots.get(self.free as usize) {
            let (at, generation) = (self.free, *generation);
            self.free = *next;
            return (at, generation);
        }
        let at = self.slots.len() as u32;
        let free = Slot::Free {
            next: NONE,
            generation: 0,
        };
        // Reserved in `reserve`.
        let _ = self.slots.try_push(free);
        (at, 0)
    }

    fn hash(&self, key: &K::View<'_>) -> u64 {
        self.hasher.hash_one(key)
    }

    fn find(&self, key: &K::View<'_>) -> Option<u32> {
        let hash = self.hash(key);
        let mut at = *self.index.get(&hash)?;
        while at != NONE {
            let entry = self.entry(at);
            if K::same(&entry.key.view(), key) {
                return Some(at);
            }
            at = entry.chain;
        }
        None
    }

    fn entry(&self, at: u32) -> &Entry<K, T> {
        match &self.slots[at as usize] {
            Slot::Used { entry, .. } => entry,
            Slot::Free { .. } => unreachable!("a free slot linked"),
        }
    }

    fn entry_mut(&mut self, at: u32) -> &mut Entry<K, T> {
        match &mut self.slots[at as usize] {
            Slot::Used { entry, .. } => entry,
            Slot::Free { .. } => unreachable!("a free slot linked"),
        }
    }

    /// Makes `at` the newest in the use queue.
    fn queue(&mut self, at: u32) {
        let newest = self.newest;
        let entry = self.entry_mut(at);
        entry.older = newest;
        entry.newer = NONE;
        if newest == NONE {
            self.oldest = at;
        } else {
            self.entry_mut(newest).newer = at;
        }
        self.newest = at;
    }

    /// Takes `at` out of the use queue, if it is in it.
    fn unqueue(&mut self, at: u32) {
        let entry = self.entry_mut(at);
        let (older, newer) = (entry.older, entry.newer);
        entry.older = NONE;
        entry.newer = NONE;
        if self.oldest == at {
            self.oldest = newer;
        } else if older != NONE {
            self.entry_mut(older).newer = newer;
        } else {
            // Not queued.
            return;
        }
        if self.newest == at {
            self.newest = older;
        } else {
            self.entry_mut(newer).older = older;
        }
    }

    /// Takes the entry at `at` out of the cache, its key and value with it.
    fn remove(&mut self, at: u32) {
        self.unqueue(at);
        let (hash, chain) = {
            let entry = self.entry(at);
            (entry.hash, entry.chain)
        };
        // Unlink it from its hash's chain.
        if self.index.get(&hash) == Some(&at) {
            if chain == NONE {
                self.index.remove(&hash);
            } else {
                self.index.insert(hash, chain);
            }
        } else {
            let mut before = self.index.get(&hash).copied().unwrap_or(NONE);
            while before != NONE {
                let next = self.entry(before).chain;
                if next == at {
                    self.entry_mut(before).chain = chain;
                    break;
                }
                before = next;
            }
        }
        let Slot::Used { generation, .. } = &self.slots[at as usize] else {
            unreachable!("a free slot linked");
        };
        // A record of the entry let go of before is stale from now on.
        let free = Slot::Free {
            next: self.free,
            generation: generation.wrapping_add(1),
        };
        let Slot::Used { entry, .. } = std::mem::replace(&mut self.slots[at as usize], free) else {
            unreachable!("a free slot linked");
        };
        self.free = at;
        self.len -= 1;
        if !matches!(entry.value, Value::Empty) {
            self.values -= 1;
        }
        debug_assert!(entry.size <= self.size);
        self.size = self.size.saturating_sub(entry.size);
        // Dropping the entry lets go of its key and value, which may log
        // other entries as released.
        drop(entry);
    }

    /// What libass counts for the entries kept.
    #[cfg(test)]
    pub(super) fn size(&self) -> usize {
        self.size
    }

    /// The entries kept.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
    }
}

/// The sizes libass counts for its values: its records, in words of the
/// target, and the pixels and outline points they hold (`ass_cache.h`,
/// `ass_bitmap.h`, `ass_outline.h`). Each takes the word size, so the
/// layouts of both targets are checked.
pub(super) mod sizes {
    const WORD: usize = size_of::<usize>();

    /// `CacheItem`: six pointers and two sizes (`ass_cache.c:330-336`).
    pub(super) const fn item(word: usize) -> usize {
        8 * word
    }

    /// `Bitmap`: four `int32_t`, a stride and a pointer.
    const fn bitmap_record(word: usize) -> usize {
        16 + 2 * word
    }

    /// `BitmapHashKey`: an outline pointer and four vectors.
    const fn bitmap_key(word: usize) -> usize {
        word + 32
    }

    /// `OutlineHashValue`: a flag, two `ASS_Outline`s of six words, a box
    /// and three `int`s, padded to a word.
    const fn outline_value(word: usize) -> usize {
        (13 * word + 28).next_multiple_of(word)
    }

    /// `CompositeHashKey`: the filter's six `int`s, a count and a pointer.
    const fn composite_key(word: usize) -> usize {
        24 + 2 * word
    }

    /// `CompositeHashValue`: three bitmaps.
    const fn composite_value(word: usize) -> usize {
        3 * bitmap_record(word)
    }

    /// `BitmapRef`: two bitmap pointers and two vectors.
    const fn bitmap_ref(word: usize) -> usize {
        2 * word + 16
    }

    /// What `ass_bitmap_construct` counts (`ass_render.c:1572-1573`) for a
    /// bitmap of `pixels` bytes, stride × height, drawn from outlines of
    /// `points` points and `segments` segments in all.
    pub(in super::super) const fn bitmap(pixels: usize, points: usize, segments: usize) -> usize {
        bitmap_on(WORD, pixels, points, segments)
    }

    const fn bitmap_on(word: usize, pixels: usize, points: usize, segments: usize) -> usize {
        (bitmap_key(word) + bitmap_record(word) + outline_value(word))
            .saturating_add(pixels)
            .saturating_add(points.saturating_mul(8))
            .saturating_add(segments)
    }

    /// What `ass_composite_construct` counts (`ass_render.c:2755-2756`) for
    /// a composite of `glyphs` glyphs whose fill, border and shadow hold
    /// `pixels` bytes.
    pub(in super::super) const fn composite(glyphs: usize, pixels: usize) -> usize {
        composite_on(WORD, glyphs, pixels)
    }

    const fn composite_on(word: usize, glyphs: usize, pixels: usize) -> usize {
        (composite_key(word) + composite_value(word))
            .saturating_add(glyphs.saturating_mul(bitmap_ref(word)))
            .saturating_add(pixels)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// libass's records as 64-bit and 32-bit ARM lay them out: `CacheItem`,
        /// `BitmapHashKey`, `Bitmap`, `OutlineHashValue`, `CompositeHashKey`,
        /// `CompositeHashValue` and `BitmapRef`.
        const LAYOUTS: [(usize, [usize; 7]); 2] = [
            (8, [64, 40, 32, 136, 40, 96, 32]),
            (4, [32, 36, 24, 80, 32, 72, 24]),
        ];

        #[test]
        fn records_are_libass_layouts_on_both_targets() {
            for (word, [item_, key, record, outline, composite, value, reference]) in LAYOUTS {
                assert_eq!(item(word), item_);
                assert_eq!(bitmap_key(word), key);
                assert_eq!(bitmap_record(word), record);
                assert_eq!(outline_value(word), outline);
                assert_eq!(composite_key(word), composite);
                assert_eq!(composite_value(word), value);
                assert_eq!(bitmap_ref(word), reference);
                assert_eq!(
                    bitmap_on(word, 100, 3, 2),
                    key + record + outline + 100 + 24 + 2
                );
                assert_eq!(
                    composite_on(word, 2, 100),
                    composite + value + 2 * reference + 100
                );
            }
        }

        #[test]
        fn counts_saturate() {
            assert_eq!(bitmap(usize::MAX, usize::MAX, 1), usize::MAX);
            assert_eq!(composite(usize::MAX, 1), usize::MAX);
        }
    }
}

/// A cache that keeps only values, never a key without one: what a lookup
/// finds is the value or nothing. The bitmap and composite caches are these;
/// libass keeps an entry for everything it draws.
#[derive(Debug)]
pub(super) struct Values<K, T>(Cache<K, T>);

impl<K: CacheKey, T> Values<K, T> {
    pub(super) fn new(budget: &Budget) -> Self {
        Self(Cache::new(budget))
    }

    /// The value kept for `key`, if one is ([`Cache::get`]).
    pub(super) fn get(&mut self, key: K::View<'_>) -> Option<Kept<T>> {
        match self.0.get(key) {
            Lookup::Hit(kept) => Some(kept),
            // Nothing here inserts an entry without a value.
            Lookup::Miss | Lookup::Empty => None,
        }
    }

    /// [`Cache::insert`].
    pub(super) fn insert(&mut self, key: K, value: T, size: usize) -> Result<Kept<T>> {
        self.0.insert(key, value, size)
    }

    /// [`Cache::cut`].
    pub(super) fn cut(&mut self, limit: usize) {
        self.0.cut(limit);
    }

    /// [`Cache::settle`].
    pub(super) fn settle(&mut self) {
        self.0.settle();
    }
}

#[cfg(test)]
mod tests {
    use triergon_core::Budget;

    use super::{CacheKey, ITEM_BYTES, Kept, Lookup};

    type Cache<T = u32> = super::Cache<u32, T>;

    impl CacheKey for u32 {
        type View<'a> = u32;

        fn view(&self) -> u32 {
            *self
        }

        fn same(a: &u32, b: &u32) -> bool {
            a == b
        }
    }

    fn cache<T>() -> Cache<T> {
        Cache::new(&Budget::new(1 << 20).unwrap())
    }

    fn found<T: Copy>(cache: &mut Cache<T>, key: u32) -> Option<T> {
        match cache.get(key) {
            Lookup::Hit(value) => Some(*value),
            _ => None,
        }
    }

    #[test]
    fn a_cut_takes_the_least_recently_used_until_it_fits() {
        let mut cache = cache();
        for key in 0..4 {
            cache.insert(key, key, 100).unwrap();
        }
        assert_eq!(found(&mut cache, 0), Some(0));
        // 1 and 2 go: the oldest uses, until two entries remain.
        cache.cut(2 * (100 + ITEM_BYTES));
        assert_eq!(cache.size(), 2 * (100 + ITEM_BYTES));
        assert!(found(&mut cache, 1).is_none() && found(&mut cache, 2).is_none());
        assert!(found(&mut cache, 0).is_some() && found(&mut cache, 3).is_some());
    }

    #[test]
    fn an_insertion_past_the_limit_cuts_as_the_frame_cut_does() {
        let mut cache = cache();
        cache.cut(2 * (100 + ITEM_BYTES));
        let held = cache.insert(0, 0, 100).unwrap();
        for key in 1..4 {
            cache.insert(key, key, 100).unwrap();
        }
        // The oldest free entries went as the newer came; the held one stays,
        // out of the queue and counted.
        assert_eq!(cache.size(), 2 * (100 + ITEM_BYTES));
        assert!(found(&mut cache, 1).is_none() && found(&mut cache, 2).is_none());
        assert!(found(&mut cache, 3).is_some());
        let Lookup::Hit(again) = cache.get(0) else {
            panic!("the held entry went");
        };
        assert!(Kept::ptr_eq(&held, &again));
    }

    #[test]
    fn a_held_entry_goes_when_its_last_holder_lets_go() {
        let mut cache = cache();
        let held = cache.insert(0, 0, 100).unwrap();
        cache.insert(1, 1, 100).unwrap();
        cache.insert(2, 2, 100).unwrap();
        // The cut passes the held entry and takes the free ones after it
        // until the size fits, which the held one alone does not.
        cache.cut(0);
        assert_eq!((cache.len(), cache.size()), (1, 100 + ITEM_BYTES));
        // The moment it is let go, it leaves the cache.
        drop(held);
        cache.settle();
        assert_eq!((cache.len(), cache.size()), (0, 0));
        assert!(matches!(cache.get(0), Lookup::Miss));
    }

    #[test]
    fn a_lookup_queues_a_held_entry_again() {
        let mut cache = cache();
        let held = cache.insert(0, 0, 100).unwrap();
        cache.cut(0);
        assert!(found(&mut cache, 0).is_some());
        // Queued again, it stays when let go until a cut takes it.
        drop(held);
        cache.cut(1000);
        assert_eq!(cache.len(), 1);
        cache.cut(0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn a_lookup_of_a_held_entry_let_go_since_finds_nothing() {
        let mut cache = cache();
        let held = cache.insert(0, 0, 100).unwrap();
        cache.cut(0);
        // Let go, but not yet taken out: the lookup takes it out.
        drop(held);
        assert!(matches!(cache.get(0), Lookup::Miss));
        assert_eq!((cache.len(), cache.size()), (0, 0));
    }

    #[test]
    fn a_stale_release_record_leaves_the_slot_s_new_entry() {
        let mut cache = cache();
        let held = cache.insert(0, 0, 100).unwrap();
        cache.cut(0);
        // Its release is recorded, and the lookup frees its slot before the
        // record is read.
        drop(held);
        assert!(matches!(cache.get(0), Lookup::Miss));
        // The next entry takes the slot while the record is pending; held,
        // it outlasts the cut that reads the record.
        let next = cache.insert(1, 1, 100).unwrap();
        cache.settle();
        assert_eq!(cache.len(), 1);
        let Lookup::Hit(found) = cache.get(1) else {
            panic!("the stale record took the new entry");
        };
        assert!(Kept::ptr_eq(&found, &next));
    }

    #[test]
    fn each_value_and_the_index_are_charged() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut cache: Cache = Cache::new(&budget);
        cache.insert(0, 0, 10).unwrap();
        let one = budget.used();
        for key in 1..100 {
            cache.insert(key, key, 10).unwrap();
        }
        let values = 99 * Cache::<u32>::NODE_BYTES;
        assert!(budget.used() >= one + values, "{} {one}", budget.used());
        // At least a bucket per entry besides.
        let buckets = 100 * Cache::<u32>::BUCKET_BYTES;
        let full = budget.used();
        assert!(full >= values + buckets);
        // The values' charges go with them.
        cache.cut(0);
        assert!(budget.used() + values <= full, "{} {full}", budget.used());
    }

    #[test]
    fn an_entry_counted_as_one_takes_no_item_record() {
        let mut cache = cache();
        for key in 0..3 {
            cache.insert(key, key, 1).unwrap();
        }
        assert_eq!(cache.size(), 3);
        cache.cut(2);
        assert!(found(&mut cache, 0).is_none());
        assert_eq!(cache.size(), 2);
    }

    #[test]
    fn an_empty_value_is_kept_and_never_held() {
        let mut cache: Cache = cache();
        cache.insert_empty(0, 10).unwrap();
        assert!(matches!(cache.get(0), Lookup::Empty));
        cache.cut(0);
        assert!(matches!(cache.get(0), Lookup::Miss));
    }

    /// Outlines whose keys hold others: G, a border B(G) whose key holds
    /// G, and X, cut to one. libass's cut finds G held and takes it out of
    /// the queue, takes B, which lets go of G, which goes too, and keeps X.
    #[test]
    fn letting_go_of_a_key_cascades_as_in_libass() {
        type Outlines = super::Cache<Key, u32>;
        #[derive(Debug)]
        enum Key {
            Glyph(u32),
            Border(Kept<u32>),
        }
        impl CacheKey for Key {
            type View<'a> = (u32, *const ());

            fn view(&self) -> (u32, *const ()) {
                match self {
                    Key::Glyph(id) => (*id, std::ptr::null()),
                    Key::Border(glyph) => (u32::MAX, Kept::as_ptr(glyph)),
                }
            }

            fn same(a: &(u32, *const ()), b: &(u32, *const ())) -> bool {
                a == b
            }
        }
        let mut cache = Outlines::new(&Budget::new(1 << 20).unwrap());
        let g = cache.insert(Key::Glyph(0), 0, 1).unwrap();
        cache.insert(Key::Border(g.clone()), 1, 1).unwrap();
        drop(g);
        cache.insert(Key::Glyph(2), 2, 1).unwrap();
        cache.cut(1);
        assert_eq!(cache.len(), 1);
        assert!(matches!(cache.get((2, std::ptr::null())), Lookup::Hit(_)));
    }

    /// Lookups leave nothing behind that holds a value: an outline a
    /// bitmap's key held goes once that bitmap does, however often it was
    /// looked up.
    #[test]
    fn lookups_do_not_hold_other_caches_values() {
        let mut outlines: Cache = cache();
        let outline = outlines.insert(0, 0, 1).unwrap();
        let mut bitmaps: super::Cache<u32, Kept<u32>> =
            super::Cache::new(&Budget::new(1 << 20).unwrap());
        bitmaps.insert(7, outline, 100).unwrap();
        for _ in 0..1000 {
            assert!(matches!(bitmaps.get(7), Lookup::Hit(_)));
        }
        outlines.cut(0);
        assert_eq!(outlines.len(), 1);
        bitmaps.cut(0);
        outlines.settle();
        assert_eq!(outlines.len(), 0);
    }

    #[test]
    fn an_exhausted_budget_leaves_the_cache_unchanged() {
        let budget = Budget::new(4096).unwrap();
        let mut cache: Cache = Cache::new(&budget);
        let mut kept = 0;
        while cache.insert(kept, kept, 10).is_ok() {
            kept += 1;
            assert!(kept < 10_000, "the budget never ran out");
        }
        // The refused insertion left nothing behind, and lookups and cuts
        // need nothing of the budget.
        assert_eq!(cache.len(), kept as usize);
        assert!(matches!(cache.get(kept), Lookup::Miss));
        assert_eq!(found(&mut cache, 0), Some(0));
        cache.cut(0);
        assert_eq!(cache.len(), 0);
        assert!(cache.insert(0, 0, 10).is_ok());
    }

    #[test]
    fn a_held_bitmap_stays_charged_until_it_is_let_go() {
        use triergon_raster::{Bitmap, Limits};
        let budget = Budget::new(1 << 20).unwrap();
        let mut cache: super::Cache<u32, Bitmap> = super::Cache::new(&budget);
        let bitmap = Bitmap::new(&budget, 64, 64, &Limits::default()).unwrap();
        let held = cache.insert(0, bitmap, 64 * 64).unwrap();
        let charged = budget.used();
        // Cut to nothing while held: it stays, counted and charged.
        cache.cut(0);
        assert_eq!(cache.size(), 64 * 64 + ITEM_BYTES);
        assert_eq!(budget.used(), charged);
        // Let go, it goes, and so does its pixels' charge.
        drop(held);
        cache.settle();
        assert_eq!(cache.size(), 0);
        assert!(
            budget.used() + 64 * 64 <= charged,
            "{} {charged}",
            budget.used()
        );
    }
}
