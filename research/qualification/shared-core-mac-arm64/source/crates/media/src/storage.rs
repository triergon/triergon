//! Immutable charged compatibility views. Cloning these handles shares storage.
use crate::{Error, Result};
use safe_media_core::{Budget, BudgetVec, Bytes};
use std::ops::{Deref, DerefMut, Range};

/// Empty values need no allocation. Nonempty spans retain their complete backing
/// allocation's charge, including bytes hidden by a slice.
#[derive(Clone, Debug, Default)]
pub struct SharedBytes(Option<Bytes>);
impl SharedBytes {
    pub fn copy_from_slice(pool: &Budget, bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        Ok(Self(Some(Bytes::copy_from_slice(pool, bytes)?)))
    }
    pub(crate) fn from_core(bytes: Bytes) -> Self {
        Self(Some(bytes))
    }
    #[cfg(test)]
    pub(crate) fn try_from_vec(pool: &Budget, bytes: Vec<u8>) -> Result<Self> {
        if bytes.capacity() == 0 {
            return Ok(Self::default());
        }
        Bytes::try_from_vec(pool, bytes)
            .map(Self::from_core)
            .map_err(|e| e.error().into())
    }
    pub fn slice(&self, range: Range<usize>) -> Result<Self> {
        match &self.0 {
            Some(bytes) => Ok(Self::from_core(bytes.slice(range)?)),
            None if range == (0..0) => Ok(Self::default()),
            None => Err(Error::Invalid("empty byte span range")),
        }
    }
    pub fn retained_capacity(&self) -> usize {
        self.0.as_ref().map_or(0, Bytes::retained_capacity)
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.0.as_ref().is_none_or(|bytes| bytes.belongs_to(pool))
    }
    pub(crate) fn owner_overhead_bound(&self) -> usize {
        if self.0.is_none() {
            return 0;
        }
        // Conservative hard-policy allowance for the byte owner's Vec,
        // reservation, Arc counters and handle. Budget counts actual charges.
        size_of::<BudgetVec<u8>>() + 2 * size_of::<usize>() + size_of::<Bytes>()
    }
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => a.shares_storage_with(b),
            (None, None) => true,
            _ => false,
        }
    }
}
impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().map_or(&[], Bytes::as_ref)
    }
}
impl Deref for SharedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl<T: AsRef<[u8]> + ?Sized> PartialEq<T> for SharedBytes {
    fn eq(&self, other: &T) -> bool {
        self.as_ref() == other.as_ref()
    }
}
impl Eq for SharedBytes {}

/// Valid UTF-8 backed by a shared charged byte span, or a static default/codec ID.
#[derive(Clone, Debug)]
pub struct SharedText {
    bytes: SharedBytes,
    constant: Option<&'static str>,
}
impl SharedText {
    pub(crate) fn constant(value: &'static str) -> Self {
        Self {
            bytes: SharedBytes::default(),
            constant: Some(value),
        }
    }
    pub(crate) fn from_utf8_span(bytes: SharedBytes) -> Result<Self> {
        std::str::from_utf8(bytes.as_ref()).map_err(|_| Error::Invalid("UTF-8 metadata"))?;
        if bytes.is_empty() {
            return Ok(Self::constant(""));
        }
        Ok(Self {
            bytes,
            constant: None,
        })
    }
    pub fn as_str(&self) -> &str {
        self.constant
            .unwrap_or_else(|| std::str::from_utf8(&self.bytes).expect("validated immutable UTF-8"))
    }
}
impl Deref for SharedText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for SharedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl<T: AsRef<str> + ?Sized> PartialEq<T> for SharedText {
    fn eq(&self, other: &T) -> bool {
        self.as_str() == other.as_ref()
    }
}
impl Eq for SharedText {}

/// Internal compatibility wrapper: slices remain convenient while all vector
/// growth goes through the core's old-plus-replacement reservation protocol.
#[derive(Debug)]
pub(crate) struct StorageVec<T>(pub(crate) BudgetVec<T>);
impl<T> StorageVec<T> {
    pub fn new(pool: &Budget) -> Self {
        Self(BudgetVec::new(pool))
    }
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<()> {
        Ok(self.0.try_reserve_exact(additional)?)
    }
    pub fn try_reserve(&mut self, additional: usize) -> Result<()> {
        let needed = self.len().checked_add(additional).ok_or(Error::Overflow)?;
        if needed <= self.capacity() {
            return Ok(());
        }
        let target = needed.max(self.capacity().saturating_mul(2)).max(4);
        let len = self.len();
        self.0.try_reserve_exact(target - len).or_else(|error| {
            if target > needed
                && matches!(
                    error,
                    safe_media_core::Error::Backpressure { .. }
                        | safe_media_core::Error::Limit { .. }
                        | safe_media_core::Error::Overflow
                )
            {
                self.0.try_reserve_exact(additional)
            } else {
                Err(error)
            }
        })?;
        Ok(())
    }
    pub fn push(&mut self, value: T) -> Result<()> {
        self.0.try_push(value).map_err(|e| e.error.into())
    }
}
impl<T> Deref for StorageVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.0.as_slice()
    }
}
impl<T> DerefMut for StorageVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.0.as_mut_slice()
    }
}

impl<'a, T> IntoIterator for &'a StorageVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl<'a, T> IntoIterator for &'a mut StorageVec<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}
/// FIFO slots carry their core allocation charge; popping moves values without
/// creating an uncharged VecDeque. Drained capacity remains charged until drop.
#[derive(Debug)]
pub(crate) struct StorageQueue<T> {
    values: StorageVec<Option<T>>,
    head: usize,
}
impl<T> StorageQueue<T> {
    pub fn new(pool: &Budget) -> Self {
        Self {
            values: StorageVec::new(pool),
            head: 0,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.head == self.values.len()
    }
    pub fn clear(&mut self) {
        self.values.clear();
        self.head = 0;
    }
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<()> {
        if self.is_empty() {
            self.clear();
        }
        self.values.try_reserve_exact(additional)
    }
    pub fn push_back(&mut self, value: T) -> Result<()> {
        if self.is_empty() {
            self.clear();
        }
        self.values.push(Some(value))
    }
    pub fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        let value = self.values[self.head].take();
        self.head += 1;
        if self.is_empty() {
            self.clear();
        }
        value
    }
}
