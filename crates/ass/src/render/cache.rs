//! What the renderer keeps between frames within a limit: outlines by
//! count, glyph bitmaps and composited runs by bytes, as libass 0.17.5
//! keeps them (`ass_cache.c`).
//!
//! libass orders a cache's entries by their last use. Cutting it to a size
//! (`ass_cache_cut`, `ass_cache.c:487-515`), which it does as each frame
//! starts, walks that order from the oldest: an entry nothing else holds
//! goes, and one still held, by the previous frame's images or by another
//! entry's key, leaves the order but stays in the cache, findable and
//! counted, until its last holder lets go (`ass_cache_dec_ref`,
//! `:469-485`) or a lookup puts it back in the order (`ass_cache_get`,
//! `:392-403`). The walk stops once the cache counts at most the limit.
//! Whatever the previous frame's images show therefore survives the cut, so
//! an equal key finds the same value in the next frame, and a value is
//! identified by itself.
//!
//! The renderer's keys hold what libass's do, but for a composite's: it
//! holds its glyphs' bitmap keys, and through them their outlines, where
//! libass's holds the bitmaps. Equal bitmap keys draw equal bitmaps, so it
//! finds the same composites, and the bitmaps a shown composite was made
//! from can leave the cache.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

use triergon_core::{Budget, BudgetVec, Error, Reservation, Result};

/// What libass counts for an entry beyond its value: its item record
/// (`CACHE_ITEM_SIZE`), six pointers and two sizes.
const ITEM_BYTES: usize = size_of::<[usize; 8]>();

/// A value a cache keeps. It is held while anything besides the cache
/// shares it; a value that holds nothing, such as a bitmap that draws
/// nothing, is never held, as libass's composites do not hold an empty
/// bitmap.
pub(super) trait Shared: Clone {
    fn is_held(&self) -> bool;
}

impl<T> Shared for Arc<T> {
    fn is_held(&self) -> bool {
        Arc::strong_count(self) > 1
    }
}

impl<T> Shared for Option<Arc<T>> {
    fn is_held(&self) -> bool {
        self.as_ref().is_some_and(Shared::is_held)
    }
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    /// What libass counts for it, its item record included.
    size: usize,
    /// Its latest use.
    used: u64,
    /// It is in the use order: an entry cut while held is not, until it
    /// is looked up again.
    ordered: bool,
}

/// A lookup or insertion of `key`. An entry's uses before its latest are
/// stale.
#[derive(Debug)]
struct Use<K> {
    used: u64,
    key: K,
}

/// Values `V` by key `K`, cut to a limit as libass cuts its caches. Its map
/// and its use order are charged to the renderer's budget; what a key or
/// value holds beyond them is charged where it is allocated.
///
/// libass cuts a cache only when a frame starts (`check_cache_limits`), so
/// whatever a frame draws stays until the next one. This cache is also cut,
/// in the same order and keeping held entries the same way, when an
/// insertion takes it past its limit and when the renderer lets go of what
/// it drew ([`Cache::settle`]), so it holds at most its limit beyond what is
/// in use. What it lets go earlier than libass is nothing a frame shows, and
/// a bitmap or composite looked up again is drawn the same under its key.
#[derive(Debug)]
pub(super) struct Cache<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// The uses, oldest first from `head`.
    uses: BudgetVec<Use<K>>,
    head: usize,
    /// Entries cut while held, which go once nothing holds them.
    held: BudgetVec<K>,
    /// The limit of the latest [`Cache::cut`].
    limit: usize,
    clock: u64,
    /// libass's `cache_size`: what the entries kept count, held ones too.
    size: usize,
    /// The map's storage.
    charge: Reservation,
}

impl<K: Clone + Eq + Hash, V: Shared> Cache<K, V> {
    /// The bytes an entry takes in the map: its key and value and the
    /// map's control byte.
    const ENTRY_BYTES: usize = size_of::<(K, Entry<V>)>() + 1;

    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            entries: HashMap::new(),
            uses: BudgetVec::new(budget),
            head: 0,
            held: BudgetVec::new(budget),
            limit: usize::MAX,
            clock: 0,
            size: 0,
            charge: Reservation::empty(budget),
        }
    }

    /// The value kept for `key`, which is now its latest use (and back in
    /// the use order if it was cut while held, as `ass_cache_get` puts it
    /// back).
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub(super) fn get<Q>(&mut self, key: &Q) -> Result<Option<V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let Some((key, entry)) = self.entries.get_key_value(key) else {
            return Ok(None);
        };
        let (key, value) = (key.clone(), entry.value.clone());
        self.used(key)?;
        Ok(Some(value))
    }

    /// Keeps `value` for `key`, counted as `size` and, unless it counts 1
    /// as an outline does, its item record (`ass_cache.c:440`), then cuts
    /// the cache to its limit if it is past it.
    ///
    /// # Errors
    ///
    /// The budget's errors; the value is then not kept.
    pub(super) fn insert(&mut self, key: K, value: V, size: usize) -> Result<V> {
        let len = self.entries.len().saturating_add(1);
        self.charge
            .resize_to(len.saturating_mul(Self::ENTRY_BYTES))?;
        self.entries.try_reserve(1).map_err(|_| Error::Allocation)?;
        let size = if size == 1 {
            1
        } else {
            size.saturating_add(ITEM_BYTES)
        };
        self.size = self.size.saturating_add(size);
        let entry = Entry {
            value: value.clone(),
            size,
            used: 0,
            ordered: true,
        };
        self.entries.insert(key.clone(), entry);
        self.used(key)?;
        self.shrink()?;
        Ok(value)
    }

    /// Cuts the cache to `limit` as `ass_cache_cut` does, and keeps it
    /// within `limit` until the next cut.
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub(super) fn cut(&mut self, limit: usize) -> Result<()> {
        self.limit = limit;
        self.shrink()
    }

    /// Cuts the cache to its limit again once the renderer has let go of
    /// what it held: an entry nothing holds now goes as libass frees one
    /// when its last holder lets go, and the cache is kept within the limit.
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub(super) fn settle(&mut self) -> Result<()> {
        self.shrink()
    }

    /// Records a use of `key`'s entry.
    fn used(&mut self, key: K) -> Result<()> {
        let Some(entry) = self.entries.get_mut(&key) else {
            return Ok(());
        };
        self.clock += 1;
        entry.used = self.clock;
        entry.ordered = true;
        self.uses
            .try_push(Use {
                used: self.clock,
                key,
            })
            .map_err(|e| e.error)?;
        // Stale uses outnumbering the entries are dropped.
        if self.uses.len() - self.head > 2 * self.entries.len() + 64 {
            self.compact()?;
        }
        Ok(())
    }

    /// Entries cut earlier while held go if nothing holds them now, as
    /// libass frees them when their last holder lets go; then, oldest use
    /// first, an entry nothing holds goes and a held one leaves the use
    /// order, until what is kept counts at most the limit.
    fn shrink(&mut self) -> Result<()> {
        let mut kept = 0;
        for i in 0..self.held.len() {
            let key = &self.held[i];
            let Some(entry) = self.entries.get(key) else {
                continue;
            };
            if entry.ordered {
                continue;
            }
            if entry.value.is_held() {
                self.held.as_mut_slice().swap(kept, i);
                kept += 1;
            } else if let Some(entry) = self.entries.remove(key) {
                self.size -= entry.size;
            }
        }
        self.held.truncate(kept);
        while self.size > self.limit && self.head < self.uses.len() {
            let Use { used, key } = &self.uses[self.head];
            self.head += 1;
            let Some(entry) = self.entries.get_mut(key) else {
                continue;
            };
            if !entry.ordered || entry.used != *used {
                continue;
            }
            if entry.value.is_held() {
                entry.ordered = false;
                self.held.try_push(key.clone()).map_err(|e| e.error)?;
            } else if let Some(entry) = self.entries.remove(key) {
                self.size -= entry.size;
            }
        }
        if self.head > self.uses.len() / 2 {
            self.compact()?;
        }
        self.charge
            .resize_to(self.entries.len().saturating_mul(Self::ENTRY_BYTES))
    }

    /// Drops the uses that are stale or behind `head`.
    fn compact(&mut self) -> Result<()> {
        let entries = &self.entries;
        let current = |u: &Use<K>| {
            entries
                .get(&u.key)
                .is_some_and(|e| e.ordered && e.used == u.used)
        };
        let mut kept = 0;
        for i in self.head..self.uses.len() {
            if current(&self.uses[i]) {
                self.uses.as_mut_slice().swap(kept, i);
                kept += 1;
            }
        }
        self.uses.truncate(kept);
        self.head = 0;
        super::fit(&mut self.uses)?;
        super::fit(&mut self.held)
    }

    /// What libass counts for the entries kept.
    #[cfg(test)]
    fn size(&self) -> usize {
        self.size
    }
}

/// The sizes libass counts for its values: its records, in words of the
/// target, and the pixels and outline points they hold (`ass_cache.h`,
/// `ass_bitmap.h`, `ass_outline.h`).
pub(super) mod sizes {
    const WORD: usize = size_of::<usize>();
    /// `Bitmap`: four `int32_t`, a stride and a pointer.
    const BITMAP: usize = 16 + 2 * WORD;
    /// `BitmapHashKey`: an outline pointer and four vectors.
    const BITMAP_KEY: usize = WORD + 32;
    /// `OutlineHashValue`: a flag, two `ASS_Outline`s of six words, a box
    /// and three `int`s, padded to a word.
    const OUTLINE_VALUE: usize = (13 * WORD + 28).next_multiple_of(WORD);
    /// `CompositeHashKey`: the filter's six `int`s, a count and a pointer.
    const COMPOSITE_KEY: usize = 24 + 2 * WORD;
    /// `CompositeHashValue`: three bitmaps.
    const COMPOSITE_VALUE: usize = 3 * BITMAP;
    /// `BitmapRef`: two bitmap pointers and two vectors.
    const BITMAP_REF: usize = 2 * WORD + 16;

    /// What `ass_bitmap_construct` counts (`ass_render.c:1572-1573`) for a
    /// bitmap of `pixels` bytes, stride × height, drawn from outlines of
    /// `points` points and `segments` segments in all.
    pub(in super::super) const fn bitmap(pixels: usize, points: usize, segments: usize) -> usize {
        BITMAP_KEY + BITMAP + pixels + OUTLINE_VALUE + 8 * points + segments
    }

    /// What `ass_composite_construct` counts (`ass_render.c:2755-2756`) for
    /// a composite of `glyphs` glyphs whose fill, border and shadow hold
    /// `pixels` bytes.
    pub(in super::super) const fn composite(glyphs: usize, pixels: usize) -> usize {
        COMPOSITE_KEY + COMPOSITE_VALUE + glyphs * BITMAP_REF + pixels
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use triergon_core::Budget;

    use super::Cache;

    const ITEM: usize = super::ITEM_BYTES;

    fn cache() -> Cache<u32, Arc<u32>> {
        Cache::new(&Budget::new(1 << 20).unwrap())
    }

    #[test]
    fn a_cut_takes_the_least_recently_used_until_it_fits() {
        let mut cache = cache();
        for key in 0..4 {
            cache.insert(key, Arc::new(key), 100).unwrap();
        }
        assert!(cache.get(&0).unwrap().is_some());
        // 1 and 2 go: the oldest uses, until two entries remain.
        cache.cut(2 * (100 + ITEM)).unwrap();
        assert_eq!(cache.size(), 2 * (100 + ITEM));
        assert!(cache.get(&1).unwrap().is_none() && cache.get(&2).unwrap().is_none());
        assert!(cache.get(&0).unwrap().is_some() && cache.get(&3).unwrap().is_some());
    }

    #[test]
    fn an_insertion_past_the_limit_cuts_as_the_frame_cut_does() {
        let mut cache = cache();
        cache.cut(2 * (100 + ITEM)).unwrap();
        let held = cache.insert(0, Arc::new(0), 100).unwrap();
        for key in 1..4 {
            cache.insert(key, Arc::new(key), 100).unwrap();
        }
        // The oldest free entries went as the newer came; the held one
        // stays, out of the use order and counted.
        assert_eq!(cache.size(), 2 * (100 + ITEM));
        assert!(cache.get(&1).unwrap().is_none() && cache.get(&2).unwrap().is_none());
        assert!(cache.get(&3).unwrap().is_some());
        assert!(Arc::ptr_eq(&held, &cache.get(&0).unwrap().unwrap()));
    }

    #[test]
    fn a_held_entry_survives_the_cut_and_counts_until_let_go() {
        let mut cache = cache();
        let held = cache.insert(0, Arc::new(0), 100).unwrap();
        cache.insert(1, Arc::new(1), 100).unwrap();
        cache.insert(2, Arc::new(2), 100).unwrap();
        // The walk passes the held entry and takes the free ones after it
        // until the size fits, which the held one alone does not.
        cache.cut(0).unwrap();
        assert_eq!(cache.size(), 100 + ITEM);
        assert!(cache.get(&1).unwrap().is_none() && cache.get(&2).unwrap().is_none());
        // Found again, it is the same value, back in the use order.
        let found = cache.get(&0).unwrap().unwrap();
        assert!(Arc::ptr_eq(&held, &found));
        drop((held, found));
        cache.cut(1000).unwrap();
        assert!(cache.get(&0).unwrap().is_some());
        // Cut while held, it goes at the first cut after it is let go.
        let held = cache.get(&0).unwrap().unwrap();
        cache.cut(0).unwrap();
        assert_eq!(cache.size(), 100 + ITEM);
        drop(held);
        cache.cut(usize::MAX).unwrap();
        assert_eq!(cache.size(), 0);
        assert!(cache.get(&0).unwrap().is_none());
    }

    #[test]
    fn a_held_bitmap_stays_charged_until_it_is_let_go() {
        use triergon_raster::{Bitmap, Limits};
        let budget = Budget::new(1 << 20).unwrap();
        let mut cache: Cache<u32, Option<Arc<Bitmap>>> = Cache::new(&budget);
        let empty = budget.used();
        let bitmap = Bitmap::new(&budget, 64, 64, &Limits::default()).unwrap();
        let held = cache.insert(0, Some(Arc::new(bitmap)), 64 * 64).unwrap();
        let charged = budget.used();
        assert!(charged >= empty + 64 * 64);
        // Cut to nothing while held: it stays, counted and charged.
        cache.cut(0).unwrap();
        assert_eq!(cache.size(), 64 * 64 + ITEM);
        let used = budget.used();
        assert!((charged..charged + 128).contains(&used), "{used} {charged}");
        // Let go, it goes at the next cut, and so do its pixels' charge
        // and its entry's; the cut keeps a little scratch.
        drop(held);
        cache.cut(0).unwrap();
        assert_eq!(cache.size(), 0);
        assert!(budget.used() < empty + 128, "{} {empty}", budget.used());
    }

    #[test]
    fn an_entry_counted_as_one_takes_no_item_record() {
        let mut cache = cache();
        for key in 0..3 {
            cache.insert(key, Arc::new(key), 1).unwrap();
        }
        assert_eq!(cache.size(), 3);
        cache.cut(2).unwrap();
        assert!(cache.get(&0).unwrap().is_none());
        assert_eq!(cache.size(), 2);
    }

    #[test]
    fn an_empty_value_is_never_held() {
        let mut cache: Cache<u32, Option<Arc<u32>>> = Cache::new(&Budget::new(1 << 20).unwrap());
        cache.insert(0, None, 10).unwrap();
        cache.cut(0).unwrap();
        assert!(cache.get(&0).unwrap().is_none());
    }
}
