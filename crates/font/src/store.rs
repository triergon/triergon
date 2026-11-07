//! A shared cache of reusable values, such as parsed faces and glyph outlines,
//! whose retained bytes are charged to a [`Budget`].

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use triergon_core::{Budget, Reservation, Result};

/// A key of the [`Store`]: an identity that is never a reusable address, and the
/// type of value it names. Keys of different types never collide.
pub trait StoreKey: Hash + Eq + Send + Sync + 'static {
    type Value: Send + Sync + 'static;
}

/// A value built for the [`Store`] and the bytes it retains beyond what its own
/// budget already charges, such as the bytes of a parsed face's input.
#[derive(Debug)]
pub struct Built<V> {
    pub value: V,
    pub bytes: usize,
}

/// Values shared under one [`Budget`].
///
/// [`Store::get_or_build`] returns the value held for a key or builds it. A new
/// value's bytes, plus a fixed bookkeeping estimate, are reserved from the budget;
/// when that fails, the least recently used values that only the store holds are
/// dropped until it succeeds. A value someone else still holds is never dropped.
/// A value that still does not fit is returned without being kept.
///
/// The store is `Send + Sync`. Building happens outside its lock, so two threads
/// may build one key at once; the first to finish is kept.
#[derive(Debug)]
pub struct Store {
    budget: Budget,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<u64, Vec<Entry>>,
    clock: u64,
}

struct Entry {
    key: Box<dyn Any + Send + Sync>,
    value: Arc<dyn Any + Send + Sync>,
    charge: Reservation,
    used: u64,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("bytes", &self.charge.bytes())
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

/// Bytes charged per entry for its key box, slot and table share.
const ENTRY_OVERHEAD: usize = 2 * size_of::<Entry>();

impl Store {
    pub fn new(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Bytes the store's values hold reserved.
    pub fn retained_bytes(&self) -> usize {
        let inner = self.lock();
        inner
            .entries
            .values()
            .flatten()
            .map(|e| e.charge.bytes())
            .sum()
    }

    /// The value held for `key`, if any.
    pub fn get<K: StoreKey>(&self, key: &K) -> Option<Arc<K::Value>> {
        let hash = hash_key(key);
        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;
        let entry = inner
            .entries
            .get_mut(&hash)?
            .iter_mut()
            .find(|entry| entry.key.downcast_ref::<K>() == Some(key))?;
        entry.used = clock;
        Arc::clone(&entry.value).downcast::<K::Value>().ok()
    }

    /// The value held for `key`, or the one `build` returns, which is kept if
    /// its bytes can be reserved.
    ///
    /// # Errors
    ///
    /// The error of `build`; nothing is kept then.
    pub fn get_or_build<K: StoreKey>(
        &self,
        key: K,
        build: impl FnOnce() -> Result<Built<K::Value>>,
    ) -> Result<Arc<K::Value>> {
        if let Some(value) = self.get(&key) {
            return Ok(value);
        }
        let Built { value, bytes } = build()?;
        let value = Arc::new(value);
        self.insert(key, &value, bytes);
        Ok(value)
    }

    /// Drops every value only the store holds.
    pub fn clear(&self) {
        let mut inner = self.lock();
        for bucket in inner.entries.values_mut() {
            bucket.retain(|entry| Arc::strong_count(&entry.value) > 1);
        }
        inner.entries.retain(|_, bucket| !bucket.is_empty());
    }

    fn insert<K: StoreKey>(&self, key: K, value: &Arc<K::Value>, bytes: usize) {
        let Some(bytes) = bytes.checked_add(ENTRY_OVERHEAD) else {
            return;
        };
        let hash = hash_key(&key);
        let mut inner = self.lock();
        if inner.entries.get(&hash).is_some_and(|bucket| {
            bucket
                .iter()
                .any(|e| e.key.downcast_ref::<K>() == Some(&key))
        }) {
            return;
        }
        let charge = loop {
            match self.budget.reserve(bytes) {
                Ok(charge) => break charge,
                Err(_) if inner.evict_one() => {}
                Err(_) => return,
            }
        };
        inner.clock += 1;
        let entry = Entry {
            key: Box::new(key),
            value: Arc::clone(value) as Arc<dyn Any + Send + Sync>,
            charge,
            used: inner.clock,
        };
        inner.entries.entry(hash).or_default().push(entry);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// Drops the least recently used value only the store holds; false if none.
    fn evict_one(&mut self) -> bool {
        let oldest = self
            .entries
            .iter()
            .flat_map(|(&hash, bucket)| {
                bucket
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                    .map(move |(index, entry)| (entry.used, hash, index))
            })
            .min();
        let Some((_, hash, index)) = oldest else {
            return false;
        };
        if let Some(bucket) = self.entries.get_mut(&hash) {
            bucket.swap_remove(index);
            if bucket.is_empty() {
                self.entries.remove(&hash);
            }
        }
        true
    }
}

fn hash_key<K: StoreKey>(key: &K) -> u64 {
    let mut hasher = DefaultHasher::new();
    TypeId::of::<K>().hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{Built, ENTRY_OVERHEAD, Store, StoreKey};
    use std::sync::Arc;
    use triergon_core::{Budget, Error, Result};

    #[derive(Debug, Hash, PartialEq, Eq)]
    struct Key(u32);
    impl StoreKey for Key {
        type Value = Vec<u8>;
    }

    #[derive(Debug, Hash, PartialEq, Eq)]
    struct Other(u32);
    impl StoreKey for Other {
        type Value = String;
    }

    fn built(bytes: usize) -> Result<Built<Vec<u8>>> {
        Ok(Built {
            value: vec![0; bytes],
            bytes,
        })
    }

    fn budget(entries: usize, bytes: usize) -> Budget {
        Budget::new(Budget::control_storage_bytes() + entries * ENTRY_OVERHEAD + bytes).unwrap()
    }

    #[test]
    fn builds_once_shares_and_separates_key_types() {
        let store = Store::new(&budget(2, 11));
        let a = store.get_or_build(Key(1), || built(10)).unwrap();
        let b = store
            .get_or_build(Key(1), || panic!("built twice"))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let text = store
            .get_or_build(Other(1), || {
                Ok(Built {
                    value: "x".to_owned(),
                    bytes: 1,
                })
            })
            .unwrap();
        assert_eq!(text.as_str(), "x");
        assert_eq!(store.retained_bytes(), 2 * ENTRY_OVERHEAD + 11);
    }

    #[test]
    fn evicts_only_values_nobody_else_holds() {
        let store = Store::new(&budget(2, 25));
        let held = store.get_or_build(Key(1), || built(10)).unwrap();
        drop(store.get_or_build(Key(2), || built(10)).unwrap());
        drop(store.get_or_build(Key(3), || built(15)).unwrap());
        assert!(store.get(&Key(1)).is_some());
        assert!(store.get(&Key(2)).is_none());
        assert!(store.get(&Key(3)).is_some());
        drop(held);
        store.clear();
        assert_eq!(store.retained_bytes(), 0);
    }

    #[test]
    fn oversized_values_are_returned_and_errors_store_nothing() {
        let store = Store::new(&budget(1, 5));
        assert_eq!(store.get_or_build(Key(1), || built(10)).unwrap().len(), 10);
        assert!(store.get(&Key(1)).is_none());
        let error = store
            .get_or_build(Key(2), || Err(Error::Invalid("test")))
            .unwrap_err();
        assert_eq!(error, Error::Invalid("test"));
        assert!(store.get(&Key(2)).is_none());
    }
}
