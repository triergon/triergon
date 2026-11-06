//! Immutable charged compatibility views. Cloning these handles shares storage.
use crate::{Error, Result};
use std::ops::{Deref, DerefMut, Range};
use triergon_core::BudgetVec;
pub use triergon_core::{Budget, Bytes, OwnedInputBytes};

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
    pub(crate) fn as_core(&self) -> Option<&Bytes> {
        self.0.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn try_from_vec(pool: &Budget, bytes: Vec<u8>) -> Result<Self> {
        if bytes.capacity() == 0 {
            return Ok(Self::default());
        }
        Bytes::try_from_vec(pool, bytes)
            .map(Self::from_core)
            .map_err(|e| e.error())
    }
    pub fn slice(&self, range: Range<usize>) -> Result<Self> {
        match &self.0 {
            Some(bytes) => Ok(Self::from_core(bytes.slice(range)?)),
            None if range == (0..0) => Ok(Self::default()),
            None => Err(Error::Invalid("empty byte span range")),
        }
    }
    /// Consumes this view without cloning its existing shared-owner handle.
    /// A successful subspan retains the complete backing allocation's charge.
    #[inline]
    pub(crate) fn into_slice(self, range: Range<usize>) -> Result<Self> {
        match self.0 {
            Some(bytes) => Ok(Self::from_core(bytes.into_slice(range)?)),
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
    pub(crate) fn new(pool: &Budget) -> Self {
        Self(BudgetVec::new(pool))
    }
    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity()
    }
    pub(crate) fn push(&mut self, value: T) -> Result<()> {
        self.0.try_push(value).map_err(|e| e.error)
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

pub(crate) mod text_job;

impl crate::matroska::typed::usage::Storage for SharedBytes {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        Ok(if index == 0 {
            self.0
                .as_ref()
                .map(|bytes| crate::matroska::typed::usage::Owner::Bytes(bytes.clone()))
        } else {
            None
        })
    }
}
crate::matroska::typed::usage::storage!(SharedText, |s| [s.bytes]);

/// Explicit typed-reader block-payload policy. Top-level metadata leaves remain
/// OwnedExact; side data inside a borrowed BlockGroup shares its actual parent.
/// SharedBounded reports the full real parent capacity/charge of borrowed views.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PayloadOwnership {
    #[default]
    OwnedExact,
    SharedBounded,
}
