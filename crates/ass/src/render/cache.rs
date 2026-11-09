//! What the renderer keeps between frames within a limit: outlines by
//! count, glyph bitmaps and composited runs by bytes, as libass 0.17.5
//! keeps them (`ass_cache.c`).
//!
//! libass orders a cache's entries by their last use. Cutting it to a size
//! (`ass_cache_cut`, `ass_cache.c:487-515`), which it does as each frame
//! starts, walks that order from the oldest: an entry nothing else holds
//! goes, and one still held, by the previous frame's images, by a composite
//! that holds its glyphs' bitmaps or by a bitmap or border that holds its
//! outline, leaves the order but stays in the cache, findable and counted,
//! until its last holder lets go (`ass_cache_dec_ref`, `:469-485`) or a
//! lookup puts it back in the order (`ass_cache_get`, `:392-403`). The walk
//! stops once the cache counts at most the limit. Whatever the previous
//! frame's images show therefore survives the cut, so an equal key finds the
//! same value in the next frame, and a value is identified by itself.

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
    /// When it was last looked up or made.
    used: u64,
    /// It is in the use order: an entry cut while held is not, until it
    /// is looked up again.
    ordered: bool,
}

/// An entry's place in the use order while the cache is cut.
#[derive(Clone, Copy, Debug)]
struct Use {
    used: u64,
    size: usize,
    held: bool,
}

/// Values `V` by key `K`, cut to a size as libass cuts its caches. Its map
/// is charged to the renderer's budget by its entries, keys and values
/// themselves; what a key or value holds beyond them is charged where it
/// is allocated.
#[derive(Debug)]
pub(super) struct Cache<K, V> {
    entries: HashMap<K, Entry<V>>,
    clock: u64,
    /// libass's `cache_size`: what the entries kept count, held ones too.
    size: usize,
    /// The map's storage.
    charge: Reservation,
    /// The use order while cutting.
    order: BudgetVec<Use>,
}

impl<K: Eq + Hash, V: Shared> Cache<K, V> {
    /// The bytes an entry takes in the map: its key, value and use record,
    /// and the map's control byte.
    const ENTRY_BYTES: usize = size_of::<(K, Entry<V>)>() + 1;

    pub(super) fn new(budget: &Budget) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            size: 0,
            charge: Reservation::empty(budget),
            order: BudgetVec::new(budget),
        }
    }

    /// The value kept for `key`, which is now its latest use.
    pub(super) fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let entry = self.entries.get_mut(key)?;
        self.clock += 1;
        entry.used = self.clock;
        entry.ordered = true;
        Some(entry.value.clone())
    }

    /// Keeps `value` for `key`, counted as `size` and, unless it counts 1
    /// as an outline does, its item record (`ass_cache.c:440`).
    ///
    /// # Errors
    ///
    /// The budget's errors; the value is then not kept.
    pub(super) fn insert(&mut self, key: K, value: V, size: usize) -> Result<V> {
        let len = self.entries.len().saturating_add(1);
        self.charge
            .resize_to(len.saturating_mul(Self::ENTRY_BYTES))?;
        self.entries.try_reserve(1).map_err(|_| Error::Allocation)?;
        self.clock += 1;
        let size = if size == 1 {
            1
        } else {
            size.saturating_add(ITEM_BYTES)
        };
        self.size = self.size.saturating_add(size);
        let entry = Entry {
            value: value.clone(),
            size,
            used: self.clock,
            ordered: true,
        };
        self.entries.insert(key, entry);
        Ok(value)
    }

    /// Cuts the cache to `limit` bytes as `ass_cache_cut` does. Entries
    /// cut earlier while held go first if nothing holds them now, as libass
    /// frees them when their last holder lets go; then, oldest use first,
    /// an entry nothing holds goes and a held one leaves the use order,
    /// until what is kept counts at most `limit`.
    ///
    /// # Errors
    ///
    /// The budget's errors.
    pub(super) fn cut(&mut self, limit: usize) -> Result<()> {
        let mut released = 0;
        self.entries.retain(|_, entry| {
            let keep = entry.ordered || entry.value.is_held();
            if !keep {
                released += entry.size;
            }
            keep
        });
        self.size -= released;
        if self.size > limit {
            self.cut_in_order(limit)?;
        }
        self.charge
            .resize_to(self.entries.len().saturating_mul(Self::ENTRY_BYTES))
    }

    fn cut_in_order(&mut self, limit: usize) -> Result<()> {
        let order = &mut self.order;
        order.clear();
        order.try_reserve_exact(self.entries.len())?;
        for entry in self.entries.values().filter(|e| e.ordered) {
            order
                .try_push(Use {
                    used: entry.used,
                    size: entry.size,
                    held: entry.value.is_held(),
                })
                .map_err(|e| e.error)?;
        }
        order.as_mut_slice().sort_unstable_by_key(|u| u.used);
        // Uses are stamped apart, so the last one the walk reaches divides
        // the entries it passes from those it does not.
        let mut last = 0;
        for u in order.as_slice() {
            last = u.used;
            if !u.held {
                self.size -= u.size;
                if self.size <= limit {
                    break;
                }
            }
        }
        self.entries.retain(|_, entry| {
            if !entry.ordered || entry.used > last {
                return true;
            }
            entry.ordered = false;
            entry.value.is_held()
        });
        self.order.clear();
        super::fit(&mut self.order)
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
        assert!(cache.get(&0).is_some());
        // 1 and 2 go: the oldest uses, until two entries remain.
        cache.cut(2 * (100 + ITEM)).unwrap();
        assert_eq!(cache.size(), 2 * (100 + ITEM));
        assert!(cache.get(&1).is_none() && cache.get(&2).is_none());
        assert!(cache.get(&0).is_some() && cache.get(&3).is_some());
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
        assert!(cache.get(&1).is_none() && cache.get(&2).is_none());
        // Found again, it is the same value, back in the use order.
        let found = cache.get(&0).unwrap();
        assert!(Arc::ptr_eq(&held, &found));
        drop((held, found));
        cache.cut(1000).unwrap();
        assert!(cache.get(&0).is_some());
        // Cut while held, it goes at the first cut after it is let go.
        let held = cache.get(&0).unwrap();
        cache.cut(0).unwrap();
        assert_eq!(cache.size(), 100 + ITEM);
        drop(held);
        cache.cut(usize::MAX).unwrap();
        assert_eq!(cache.size(), 0);
        assert!(cache.get(&0).is_none());
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
        assert!(cache.get(&0).is_none());
        assert_eq!(cache.size(), 2);
    }

    #[test]
    fn an_empty_value_is_never_held() {
        let mut cache: Cache<u32, Option<Arc<u32>>> = Cache::new(&Budget::new(1 << 20).unwrap());
        cache.insert(0, None, 10).unwrap();
        cache.cut(0).unwrap();
        assert!(cache.get(&0).is_none());
    }
}
