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

/// A shared allocation's address. It can be reused after the last owner drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteStorageAddress(usize);

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
    /// Whether another live view retains this allocation.
    pub fn has_shared_storage(&self) -> bool {
        Arc::strong_count(&self.storage) != 1
    }
    /// Compare backing allocations without extending their lifetimes.
    pub fn storage_address(&self) -> ByteStorageAddress {
        ByteStorageAddress(Arc::as_ptr(&self.storage).addr())
    }
    pub fn belongs_to(&self, budget: &Budget) -> bool {
        self.storage.charge.budget.same_pool(budget)
    }
    /// Clear a uniquely owned full allocation for another write. A shared
    /// owner or a slice is returned intact.
    pub fn try_reuse(mut self) -> std::result::Result<BytesWriter, Self> {
        if self.range != (0..self.storage.data.len()) {
            return Err(self);
        }
        let Some(storage) = Arc::get_mut(&mut self.storage) else {
            return Err(self);
        };
        storage.data.clear();
        Ok(BytesWriter(self.storage))
    }

    /// Convert an exclusive, complete view into charged mutable storage.
    /// Retains the allocation, contents and capacity charge. Releases the
    /// shared-header charge after disposing of that header. Rejection returns
    /// the original shared or partial view with its complete charge.
    pub fn try_into_buffer(self) -> std::result::Result<BudgetVec<u8>, Self> {
        if self.range != (0..self.storage.data.len()) {
            return Err(self);
        }
        let Self { storage, range } = self;
        match Arc::try_unwrap(storage) {
            Ok(Storage { data, mut charge }) => {
                // The Arc allocation has been released before its charge.
                charge.shrink_by(STORAGE_BYTES);
                Ok(BudgetVec { data, charge })
            }
            Err(storage) => Err(Self { storage, range }),
        }
    }
}

/// Exclusive access to an existing charged byte allocation. Its capacity
/// includes the shared owner, so publishing the new bytes needs no allocation.
#[derive(Debug)]
pub struct BytesWriter(Arc<Storage>);

impl BytesWriter {
    pub fn capacity(&self) -> usize {
        self.0.data.capacity()
    }

    pub fn belongs_to(&self, budget: &Budget) -> bool {
        self.0.charge.budget.same_pool(budget)
    }

    pub fn append_repeated(&mut self, byte: u8, count: usize) -> Result<()> {
        self.append_runs([(byte, count)])
    }

    /// Append runs to this allocation. Check the combined length before
    /// writing any pixels so a rejected append leaves the writer intact.
    pub fn append_runs<const N: usize>(&mut self, runs: [(u8, usize); N]) -> Result<()> {
        let data = &mut Arc::get_mut(&mut self.0)
            .expect("byte writer is the sole owner")
            .data;
        let end = runs.iter().try_fold(data.len(), |len, &(_, count)| {
            len.checked_add(count).ok_or(Error::Overflow)
        })?;
        if end > data.capacity() {
            return Err(Error::Limit {
                requested: end,
                maximum: data.capacity(),
            });
        }
        for (byte, count) in runs {
            data.resize(data.len() + count, byte);
        }
        Ok(())
    }

    pub fn finish(self) -> Bytes {
        Bytes {
            range: 0..self.0.data.len(),
            storage: self.0,
        }
    }
}

/// Capability for caller-provided immutable input storage. Construction only
/// adopts a caller Vec; a produced Bytes/packet/metadata view cannot be converted
/// into this capability. Its complete capacity and shared header stay charged
/// while any input-window/output aliases exist.
///
/// ```compile_fail
/// use triergon_core::{Budget, Bytes, OwnedInputBytes};
/// let pool = Budget::new(4096).unwrap();
/// let produced = Bytes::copy_from_slice(&pool, b"packet").unwrap();
/// let input = OwnedInputBytes::from(produced);
/// ```
///
/// ```compile_fail
/// use triergon_core::{Budget, Bytes};
/// let pool = Budget::new(4096).unwrap();
/// let produced = Bytes::copy_from_slice(&pool, b"packet").unwrap();
/// let vector = produced.try_into_vec();
/// ```
#[repr(transparent)]
#[derive(Debug)]
pub struct OwnedInputBytes {
    bytes: Bytes,
}
impl OwnedInputBytes {
    /// Adopts an existing caller allocation, returning that same Vec on pressure.
    pub fn try_from_vec(
        budget: &Budget,
        input: Vec<u8>,
    ) -> std::result::Result<Self, TransferError> {
        Bytes::try_from_vec(budget, input).map(|bytes| Self { bytes })
    }
    /// Read-only input identity. Cloning a view retains its whole original charge.
    pub fn as_bytes(&self) -> &Bytes {
        &self.bytes
    }
    /// Returns the original caller input allocation only after every produced
    /// view drops. This capability cannot be made from a produced Bytes owner.
    /// No copy/allocation occurs. Rejection preserves this capability and charge.
    pub fn try_into_vec(mut self) -> std::result::Result<Vec<u8>, Self> {
        if self.bytes.range.start != 0
            || self.bytes.range.end != self.bytes.storage.data.len()
            || Arc::get_mut(&mut self.bytes.storage).is_none()
        {
            return Err(self);
        }
        let Self {
            bytes: Bytes { storage, range },
        } = self;
        match Arc::try_unwrap(storage) {
            Ok(Storage { data, charge }) => {
                drop(charge);
                Ok(data)
            }
            Err(storage) => Err(Self {
                bytes: Bytes { storage, range },
            }),
        }
    }
}
impl AsRef<[u8]> for OwnedInputBytes {
    fn as_ref(&self) -> &[u8] {
        self.bytes.as_ref()
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
    /// `len` copies of `value`, charged before they are allocated. A refused
    /// charge or allocation is an error, never an abort.
    pub fn try_filled(budget: &Budget, value: T, len: usize) -> Result<Self>
    where
        T: Clone,
    {
        let mut filled = Self::new(budget);
        filled.try_resize(len, value)?;
        Ok(filled)
    }
    /// A copy with its own charge on the same pool.
    pub fn try_clone(&self) -> Result<Self>
    where
        T: Clone,
    {
        let mut copy = Self::new(self.budget());
        copy.grow_to(self.len())?;
        copy.data.extend_from_slice(&self.data);
        Ok(copy)
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    /// The pool this vector's capacity is charged to.
    pub fn budget(&self) -> &Budget {
        &self.charge.budget
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
    /// Shortens the vector; its allocation and charge are kept.
    pub fn truncate(&mut self, len: usize) {
        self.data.truncate(len);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.data.pop()
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
        // The replacement is charged on top of the current allocation's charge
        // for the whole copy, and the old part is returned only after the old
        // allocation drops: the same pool totals as a separate reservation,
        // without another pool handle.
        let old = self.charge.bytes();
        self.charge.grow_by(size)?;
        let mut replacement = Vec::new();
        let charged = replacement
            .try_reserve_exact(target)
            .map_err(|_| Error::Allocation)
            .and_then(|()| {
                replacement
                    .capacity()
                    .checked_mul(size_of::<T>())
                    .ok_or(Error::Overflow)
            })
            .and_then(|bytes| self.charge.grow_by(bytes - size));
        if let Err(error) = charged {
            drop(replacement);
            self.charge.shrink_by(size);
            return Err(error);
        }
        replacement.append(&mut self.data);
        let previous = std::mem::replace(&mut self.data, replacement);
        drop(previous);
        self.charge.shrink_by(old);
        Ok(())
    }
    #[inline]
    pub fn try_push(&mut self, value: T) -> std::result::Result<(), PushError<T>> {
        if self.len() == self.capacity()
            && let Err(error) = self.grow_for_push()
        {
            return Err(PushError { error, value });
        }
        self.data.push(value);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn grow_for_push(&mut self) -> Result<()> {
        let needed = self.len().checked_add(1).ok_or(Error::Overflow)?;
        let target = needed.max(self.capacity().saturating_mul(2).max(4));
        self.grow_to(target).or_else(|error| {
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
        })
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
    /// Reserve space for all new elements before constructing any of them.
    pub fn try_extend_with(&mut self, count: usize, make: impl FnMut(usize) -> T) -> Result<()> {
        self.try_reserve_exact(count)?;
        self.data.extend((0..count).map(make));
        Ok(())
    }
    /// Move elements after reserving destination capacity. Both buffers stay
    /// charged to their existing pools. A failed reserve leaves both intact.
    pub fn try_append(&mut self, other: &mut Self) -> Result<()> {
        self.try_reserve_exact(other.len())?;
        self.data.append(&mut other.data);
        Ok(())
    }

    /// Initializes new elements only after reserving their complete capacity.
    /// Shrinking retains the allocation and its existing charge.
    pub fn try_resize(&mut self, new_len: usize, value: T) -> Result<()>
    where
        T: Clone,
    {
        self.grow_to(new_len)?;
        self.data.resize(new_len, value);
        Ok(())
    }
}

impl<T> std::ops::Deref for BudgetVec<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        &self.data
    }
}

impl<T> std::ops::DerefMut for BudgetVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.data
    }
}

/// Consuming traversal of a charged vector. Its allocation stays charged until
/// the iterator releases it, including when only part of the vector is visited.
#[derive(Debug)]
pub struct BudgetVecIntoIter<T> {
    iter: std::vec::IntoIter<T>,
    _charge: Reservation,
}

impl<T> Iterator for BudgetVecIntoIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<T> ExactSizeIterator for BudgetVecIntoIter<T> {}

impl<T> IntoIterator for BudgetVec<T> {
    type Item = T;
    type IntoIter = BudgetVecIntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        BudgetVecIntoIter {
            iter: self.data.into_iter(),
            _charge: self.charge,
        }
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

/// Length of the leading run of nonzero bytes.
#[inline]
pub fn nonzero_prefix_len(bytes: &[u8]) -> usize {
    let mut words = bytes.chunks_exact(8);
    for (index, chunk) in words.by_ref().enumerate() {
        let word = u64::from_le_bytes(chunk.try_into().expect("eight-byte chunk"));
        let zeroes = word.wrapping_sub(0x0101_0101_0101_0101) & !word & 0x8080_8080_8080_8080;
        if zeroes != 0 {
            // Borrow can mark a following byte, so use the lowest marked octet.
            return index * 8 + zeroes.trailing_zeros() as usize / 8;
        }
    }
    let tail = words.remainder();
    bytes.len() - tail.len()
        + tail
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(tail.len())
}

#[cfg(test)]
mod tests {
    use super::{BudgetVec, Bytes, nonzero_prefix_len};
    use crate::Budget;

    #[test]
    fn complete_bytes_transfer_the_allocation_and_release_only_the_shared_header() {
        let pool = Budget::new(4096).unwrap();
        let mut buffer = BudgetVec::new(&pool);
        buffer.try_reserve_exact(64).unwrap();
        buffer.try_extend_from_slice(b"frame").unwrap();
        let bytes = buffer.freeze().unwrap();
        let pointer = bytes.as_ref().as_ptr();
        let capacity = bytes.retained_capacity();
        let pressure = pool.reserve(pool.available()).unwrap();
        let before = pool.used();
        let mut buffer = bytes.try_into_buffer().unwrap();
        assert_eq!(buffer.as_slice(), b"frame");
        assert_eq!(buffer.as_slice().as_ptr(), pointer);
        assert_eq!(buffer.capacity(), capacity);
        assert!(buffer.budget().same_pool(&pool));
        assert_eq!(pool.used(), before - Bytes::owner_overhead_bytes());
        buffer.clear();
        buffer.try_extend_from_slice(b"next").unwrap();
        let bytes = buffer.freeze().unwrap();
        assert_eq!(bytes.as_ref(), b"next");
        assert_eq!(bytes.as_ref().as_ptr(), pointer);
        assert_eq!(pool.used(), before);
        drop(pressure);
        drop(bytes);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn shared_bytes_reject_buffer_conversion_without_changes() {
        let pool = Budget::new(4096).unwrap();
        let bytes = Bytes::copy_from_slice(&pool, b"frame").unwrap();
        let alias = bytes.clone();
        let before = pool.used();
        let bytes = bytes.try_into_buffer().unwrap_err();
        assert!(bytes.shares_storage_with(&alias));
        assert_eq!(bytes.as_ref(), b"frame");
        assert_eq!(pool.used(), before);
        drop(alias);
        assert_eq!(bytes.try_into_buffer().unwrap().as_slice(), b"frame");
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn exclusive_slices_reject_buffer_conversion_without_changes() {
        let pool = Budget::new(4096).unwrap();
        for range in [0..3, 1..4, 1..3, 2..2] {
            let bytes = Bytes::copy_from_slice(&pool, b"data")
                .unwrap()
                .into_slice(range.clone())
                .unwrap();
            let address = bytes.storage_address();
            let before = pool.used();
            assert!(!bytes.has_shared_storage());
            let bytes = bytes.try_into_buffer().unwrap_err();
            assert_eq!(bytes.storage_address(), address);
            assert_eq!(bytes.as_ref(), &b"data"[range]);
            assert_eq!(bytes.retained_capacity(), 4);
            assert_eq!(pool.used(), before);
            drop(bytes);
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }

    #[test]
    fn nonzero_prefix_stops_at_the_first_zero_in_groups_and_tails() {
        for fill in [1, 0x80, 0xff] {
            for offset in 0..16 {
                for length in 0..=80 {
                    for zero in 0..=length {
                        let mut data = [fill; 97];
                        data[offset + zero] = 0;
                        let view = &data[offset..offset + length];
                        assert_eq!(nonzero_prefix_len(view), zero);
                    }
                }
            }
        }
    }
}
