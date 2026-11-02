//! Immutable shared spans and charged mutable construction/arena storage.
use crate::{Budget, Error, Reservation, Result};
use std::{ops::Range, sync::Arc};

#[derive(Debug)]
struct Storage {
    // Field drop order releases the allocation before returning its charge.
    data: Vec<u8>,
    charge: Reservation,
}
const STORAGE_BYTES: usize = size_of::<Storage>() + 2 * size_of::<usize>();

/// A checked immutable span. All aliases keep one whole backing capacity charged.
#[derive(Clone, Debug)]
pub struct Bytes {
    storage: Arc<Storage>,
    range: Range<usize>,
}

impl Bytes {
    pub fn copy_from_slice(budget: &Budget, input: &[u8]) -> Result<Self> {
        let size = input
            .len()
            .checked_add(STORAGE_BYTES)
            .ok_or(Error::Overflow)?;
        let mut charge = budget.reserve(size)?;
        let mut data = Vec::new();
        data.try_reserve_exact(input.len())
            .map_err(|_| Error::Allocation)?;
        charge.grow_to(
            data.capacity()
                .checked_add(STORAGE_BYTES)
                .ok_or(Error::Overflow)?,
        )?;
        data.extend_from_slice(input);
        Ok(Self::from_storage(data, charge))
    }
    /// Transfers caller-owned storage. Rejection returns that exact allocation.
    pub fn try_from_vec(
        budget: &Budget,
        input: Vec<u8>,
    ) -> std::result::Result<Self, TransferError> {
        let charge = input
            .capacity()
            .checked_add(STORAGE_BYTES)
            .ok_or(Error::Overflow)
            .and_then(|size| budget.reserve(size));
        match charge {
            Ok(charge) => Ok(Self::from_storage(input, charge)),
            Err(error) => Err(TransferError { error, input }),
        }
    }
    fn from_storage(data: Vec<u8>, charge: Reservation) -> Self {
        let range = 0..data.len();
        Self {
            storage: Arc::new(Storage { data, charge }),
            range,
        }
    }
    pub fn slice(&self, range: Range<usize>) -> Result<Self> {
        let range = self.subrange(range)?;
        Ok(Self {
            storage: Arc::clone(&self.storage),
            range,
        })
    }
    /// Consumes this handle and selects a checked subspan without cloning or
    /// allocating a new shared owner. The complete backing capacity and header
    /// remain charged, including for an empty successful view. Bounds are
    /// relative to this view. An invalid range drops the consumed handle;
    /// other aliases remain valid and retain their existing charge.
    pub fn into_slice(mut self, range: Range<usize>) -> Result<Self> {
        self.range = self.subrange(range)?;
        Ok(self)
    }
    fn subrange(&self, range: Range<usize>) -> Result<Range<usize>> {
        if range.start > range.end || range.end > self.len() {
            return Err(Error::Invalid("byte span range"));
        }
        let start = self
            .range
            .start
            .checked_add(range.start)
            .ok_or(Error::Overflow)?;
        let end = self
            .range
            .start
            .checked_add(range.end)
            .ok_or(Error::Overflow)?;
        Ok(start..end)
    }
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
    pub fn retained_capacity(&self) -> usize {
        self.storage.data.capacity()
    }
    /// Complete immutable owner's tracked charge, including backing capacity
    /// and the fixed shared-owner header. Every slice reports the same charge;
    /// use `shares_storage_with` to deduplicate aliases. This is not process RSS.
    pub fn retained_storage_bytes(&self) -> usize {
        self.storage.charge.bytes()
    }
    /// Fixed tracked header charge required when freezing mutable bytes into
    /// an immutable shared owner, independently of its backing capacity.
    pub const fn owner_overhead_bytes() -> usize {
        STORAGE_BYTES
    }
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
    pub fn belongs_to(&self, budget: &Budget) -> bool {
        self.storage.charge.budget.same_pool(budget)
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.storage.data[self.range.clone()]
    }
}
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

#[derive(Debug)]
pub struct TransferError {
    error: Error,
    input: Vec<u8>,
}
impl TransferError {
    pub fn error(&self) -> Error {
        self.error
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.input
    }
}

/// Capacity-accounted mutable storage. Growth temporarily charges both old and
/// replacement buffers, preserving contents exactly if reservation/allocation fails.
#[derive(Debug)]
pub struct BudgetVec<T> {
    data: Vec<T>,
    charge: Reservation,
}

#[derive(Debug)]
pub struct PushError<T> {
    pub error: Error,
    pub value: T,
}

impl<T> BudgetVec<T> {
    pub fn new(budget: &Budget) -> Self {
        Self {
            data: Vec::new(),
            charge: Reservation::empty(budget),
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }
    pub fn clear(&mut self) {
        self.data.clear();
    }
    #[inline]
    pub fn try_reserve_exact(&mut self, additional: usize) -> Result<()> {
        let target = self.len().checked_add(additional).ok_or(Error::Overflow)?;
        self.grow_to(target)
    }
    #[inline]
    fn grow_to(&mut self, target: usize) -> Result<()> {
        if target <= self.capacity() {
            return Ok(());
        }
        self.grow_allocation(target)
    }
    #[cold]
    #[inline(never)]
    fn grow_allocation(&mut self, target: usize) -> Result<()> {
        let size = target.checked_mul(size_of::<T>()).ok_or(Error::Overflow)?;
        let mut charge = self.charge.budget.reserve(size)?;
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(target)
            .map_err(|_| Error::Allocation)?;
        charge.grow_to(
            replacement
                .capacity()
                .checked_mul(size_of::<T>())
                .ok_or(Error::Overflow)?,
        )?;
        replacement.append(&mut self.data);
        let old = std::mem::replace(&mut self.data, replacement);
        drop(old);
        self.charge = charge;
        Ok(())
    }
    pub fn try_push(&mut self, value: T) -> std::result::Result<(), PushError<T>> {
        if self.len() == self.capacity() {
            let needed = match self.len().checked_add(1) {
                Some(value) => value,
                None => {
                    return Err(PushError {
                        error: Error::Overflow,
                        value,
                    });
                }
            };
            let target = needed.max(self.capacity().saturating_mul(2).max(4));
            let result = self.grow_to(target).or_else(|error| {
                if target != needed
                    && matches!(
                        error,
                        Error::Backpressure { .. } | Error::Limit { .. } | Error::Overflow
                    )
                {
                    self.grow_to(needed)
                } else {
                    Err(error)
                }
            });
            if let Err(error) = result {
                return Err(PushError { error, value });
            }
        }
        self.data.push(value);
        Ok(())
    }
    #[inline]
    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<()>
    where
        T: Copy,
    {
        if values.len() == 1 && self.len() < self.capacity() {
            self.data.push(values[0]);
            return Ok(());
        }
        self.try_reserve_exact(values.len())?;
        self.data.extend_from_slice(values);
        Ok(())
    }
    /// Initializes new elements only after reserving their complete capacity.
    /// Shrinking retains the allocation and its existing charge.
    pub fn try_resize(&mut self, new_len: usize, value: T) -> Result<()>
    where
        T: Copy,
    {
        self.grow_to(new_len)?;
        self.data.resize(new_len, value);
        Ok(())
    }
}

#[derive(Debug)]
pub struct FreezeError {
    pub error: Error,
    pub buffer: BudgetVec<u8>,
}
impl BudgetVec<u8> {
    /// Appends an LZ-style repetition from already materialized history. The
    /// destination is reserved before any bytes change, including when the
    /// repeated source overlaps the destination. Callers bound copy work.
    #[inline]
    pub fn try_extend_from_history(&mut self, distance: usize, count: usize) -> Result<()> {
        if distance == 0 || distance > self.len() {
            return Err(Error::Invalid("history distance"));
        }
        let start = self.len() - distance;
        let end = self.len().checked_add(count).ok_or(Error::Overflow)?;
        if end > self.capacity() {
            self.grow_to(end)?;
        }
        // A unary run has no changing source. Initializing the new range once
        // avoids repeatedly moving the growing prefix, with the same upfront
        // capacity charge and rejection behavior as general history copies.
        if distance == 1 {
            self.data.resize(end, self.data[start]);
            return Ok(());
        }
        while self.len() < end {
            let count = (self.len() - start).min(end - self.len());
            self.data.extend_from_within(start..start + count);
        }
        Ok(())
    }
    /// Moves the existing allocation into shared immutable storage, with no copy
    /// and no interval in which the allocation loses its charge.
    pub fn freeze(mut self) -> std::result::Result<Bytes, FreezeError> {
        let result = self
            .capacity()
            .checked_add(STORAGE_BYTES)
            .ok_or(Error::Overflow)
            .and_then(|size| self.charge.grow_to(size));
        if let Err(error) = result {
            return Err(FreezeError {
                error,
                buffer: self,
            });
        }
        let Self { data, charge } = self;
        Ok(Bytes::from_storage(data, charge))
    }
}
