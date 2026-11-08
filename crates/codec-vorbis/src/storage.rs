//! Budget-charged allocation of stream-sized storage.

use triergon_codec::{Reservation, StorageError};

/// A charge that grows before each allocation it covers.
#[derive(Debug)]
pub(crate) struct Meter(pub(crate) Reservation);

impl Meter {
    /// Charges `count` more values of `T`.
    pub(crate) fn add<T>(&mut self, count: usize) -> Result<(), StorageError> {
        let bytes = count
            .checked_mul(size_of::<T>())
            .and_then(|bytes| bytes.checked_add(self.0.bytes()))
            .ok_or(StorageError::Overflow)?;
        self.0.resize_to(bytes)
    }

    /// Returns the charge of `count` values of `T` whose storage is gone.
    pub(crate) fn release<T>(&mut self, count: usize) {
        let bytes = self
            .0
            .bytes()
            .saturating_sub(count.saturating_mul(size_of::<T>()));
        // Shrinking never fails.
        let _ = self.0.resize_to(bytes);
    }

    /// `len` copies of `value`, charged first.
    pub(crate) fn vec<T: Clone>(&mut self, len: usize, value: T) -> Result<Vec<T>, StorageError> {
        let mut out = self.with_capacity(len)?;
        out.resize(len, value);
        Ok(out)
    }

    /// An empty vector with room for `len` values, charged first.
    pub(crate) fn with_capacity<T>(&mut self, len: usize) -> Result<Vec<T>, StorageError> {
        self.add::<T>(len)?;
        let mut out = Vec::new();
        out.try_reserve_exact(len)
            .map_err(|_| StorageError::Allocation)?;
        Ok(out)
    }
}
