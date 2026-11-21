//! The accounting pool every input-dependent allocation is charged to.
use crate::{Error, Result};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct Inner {
    id: PoolId,
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
    generation: AtomicUsize,
}

/// A pool with a fixed byte limit that every live allocation is charged to.
///
/// Clones share one account. A charge is a [`Reservation`] and lasts until it
/// drops, wherever the storage it covers has travelled: output a caller keeps
/// stays charged. The counters are requested capacity, not allocator overhead
/// or process memory.
#[derive(Clone, Debug)]
pub struct Budget(Arc<Inner>);

const CONTROL_BYTES: usize = size_of::<Inner>() + 2 * size_of::<usize>();

// Tokens identify a pool without keeping its allocation alive. A process-wide,
// checked serial cannot alias a new pool when an old Arc address is reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PoolId(usize);

static NEXT_POOL_ID: AtomicUsize = AtomicUsize::new(0);

fn allocate_pool_id(counter: &AtomicUsize) -> Result<PoolId> {
    let mut previous = counter.load(Ordering::Relaxed);
    loop {
        let next = previous.checked_add(1).ok_or(Error::Overflow)?;
        match counter.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(PoolId(next)),
            Err(observed) => previous = observed,
        }
    }
}

impl Budget {
    /// Bytes every pool charges itself for its own shared state: what
    /// [`used`](Self::used) reports for an empty pool.
    pub const fn control_storage_bytes() -> usize {
        CONTROL_BYTES
    }
    /// A pool that holds at most `limit` bytes, its own
    /// [`control_storage_bytes`](Self::control_storage_bytes) included. Fails
    /// with [`Error::Limit`] when `limit` is below those.
    pub fn new(limit: usize) -> Result<Self> {
        if limit < CONTROL_BYTES {
            return Err(Error::Limit {
                requested: CONTROL_BYTES,
                maximum: limit,
            });
        }
        Ok(Self(Arc::new(Inner {
            id: allocate_pool_id(&NEXT_POOL_ID)?,
            limit,
            used: AtomicUsize::new(CONTROL_BYTES),
            peak: AtomicUsize::new(CONTROL_BYTES),
            generation: AtomicUsize::new(0),
        })))
    }
    /// The limit the pool was made with, in bytes.
    pub fn limit(&self) -> usize {
        self.0.limit
    }
    /// Bytes charged now.
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Relaxed)
    }
    /// The most bytes that were ever charged at once.
    pub fn peak(&self) -> usize {
        self.0.peak.load(Ordering::Relaxed)
    }
    /// Bytes still free: [`limit`](Self::limit) less [`used`](Self::used).
    pub fn available(&self) -> usize {
        self.limit() - self.used()
    }
    /// Whether `other` is a clone of this pool.
    pub fn same_pool(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn identity(&self) -> PoolId {
        self.0.id
    }
    /// A generation no cursor of this pool has had.
    pub(crate) fn next_generation(&self) -> Result<u64> {
        let mut current = self.0.generation.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(1).ok_or(Error::Overflow)?;
            let generation = u64::try_from(next).map_err(|_| Error::Overflow)?;
            match self.0.generation.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(generation),
                Err(value) => current = value,
            }
        }
    }
    /// Charges `bytes`. Fails with [`Error::Limit`] when they exceed what an
    /// empty pool could hold, and with [`Error::Backpressure`] when they fit
    /// only once other charges are released; neither changes the pool.
    pub fn reserve(&self, bytes: usize) -> Result<Reservation> {
        self.claim(bytes)?;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
    fn claim(&self, bytes: usize) -> Result<()> {
        let maximum = self.limit() - CONTROL_BYTES;
        if bytes > maximum {
            return Err(Error::Limit {
                requested: bytes,
                maximum,
            });
        }
        if bytes == 0 {
            return Ok(());
        }
        let mut used = self.used();
        loop {
            let available = self.limit() - used;
            if bytes > available {
                return Err(Error::Backpressure {
                    requested: bytes,
                    available,
                });
            }
            let total = used + bytes; // bounded by limit above; cannot overflow
            match self.0.used.compare_exchange_weak(
                used,
                total,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // The high-water mark never decreases, so an observed peak at
                    // or above this total already records it without another RMW.
                    if total > self.0.peak.load(Ordering::Relaxed) {
                        self.0.peak.fetch_max(total, Ordering::Relaxed);
                    }
                    return Ok(());
                }
                Err(current) => used = current,
            }
        }
    }
}

/// A charge of some bytes on a [`Budget`], returned to it on drop.
///
/// It is not `Clone`: one charge covers one allocation, and lives in that
/// allocation's owner so both end together.
#[derive(Debug)]
pub struct Reservation {
    pub(crate) budget: Budget,
    bytes: usize,
}

impl Reservation {
    /// Bytes charged.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    /// Changes the charge to exactly `bytes`, for an owner that keeps its
    /// storage in plain containers. Growth claims the difference as
    /// [`Budget::reserve`] would and leaves the charge unchanged on failure;
    /// shrinking returns the difference to the pool and cannot fail.
    pub fn resize_to(&mut self, bytes: usize) -> Result<()> {
        if bytes < self.bytes {
            self.shrink_by(self.bytes - bytes);
            Ok(())
        } else {
            self.grow_to(bytes)
        }
    }
    /// A zero-byte charge on `budget`, for an owner that sizes it later with
    /// [`resize_to`](Self::resize_to).
    pub fn empty(budget: &Budget) -> Self {
        Self {
            budget: budget.clone(),
            bytes: 0,
        }
    }
    /// Returns part of this charge to its pool once the storage it covered is gone.
    pub(crate) fn shrink_by(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.bytes);
        if bytes != 0 {
            self.budget.0.used.fetch_sub(bytes, Ordering::Relaxed);
            self.bytes -= bytes;
        }
    }
    /// Claims `extra` more bytes, as [`Budget::reserve`] would.
    pub(crate) fn grow_by(&mut self, extra: usize) -> Result<()> {
        self.budget.claim(extra)?;
        // Both terms are bounded by the pool limit after a successful claim.
        self.bytes += extra;
        Ok(())
    }
    /// Grows the charge to `bytes`, which must not be less than it holds.
    pub(crate) fn grow_to(&mut self, bytes: usize) -> Result<()> {
        if bytes < self.bytes {
            return Err(Error::Invalid("reservation growth shrinks"));
        }
        if bytes != self.bytes {
            self.budget.claim(bytes - self.bytes)?;
            self.bytes = bytes;
        }
        Ok(())
    }
}

/// A value with the charge for its storage, released together: state a stream
/// creates per element, such as a substream, channel element or extension
/// decoder. It dereferences to the value.
#[derive(Debug)]
pub struct Charged<T> {
    value: T,
    charge: Reservation,
}

impl<T> Charged<T> {
    /// Charges `bytes` to `budget`, then builds the value; `make` does not run
    /// when the charge is refused.
    pub fn try_new(budget: &Budget, bytes: usize, make: impl FnOnce() -> T) -> Result<Self> {
        let charge = budget.reserve(bytes)?;
        Ok(Self {
            value: make(),
            charge,
        })
    }
    /// `value` in a box charged `size_of::<T>()` bytes. Storage the value owns
    /// elsewhere is not counted; it charges itself, as a
    /// [`BudgetVec`](crate::BudgetVec) does.
    pub fn boxed(budget: &Budget, value: T) -> Result<Charged<Box<T>>> {
        Charged::try_new(budget, size_of::<T>(), || Box::new(value))
    }
    /// Bytes charged for this value.
    pub fn charged_bytes(&self) -> usize {
        self.charge.bytes()
    }
}

impl<T> std::ops::Deref for Charged<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> std::ops::DerefMut for Charged<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.budget.0.used.fetch_sub(self.bytes, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;
    use crate::{InputCursor, SourceCapabilities, SourceId};

    #[test]
    fn request_tokens_do_not_retain_the_allocation_pool() {
        let pool = Budget::new(4096).unwrap();
        let cursor = InputCursor::new(
            &pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access: true,
            },
            16,
        )
        .unwrap();
        let owners = Arc::strong_count(&pool.0);
        let first = cursor.request();
        let second = first.clone();
        let third = cursor.request();
        assert_eq!(
            Arc::strong_count(&pool.0),
            owners,
            "request identity must not add atomic allocation-pool owners"
        );
        assert_eq!(first.token, second.token);
        assert_eq!(first.token, third.token);
    }

    #[test]
    fn exhausted_pool_identity_counter_never_wraps_or_reuses_an_id() {
        let counter = AtomicUsize::new(usize::MAX - 1);
        assert_eq!(allocate_pool_id(&counter).unwrap(), PoolId(usize::MAX));
        assert_eq!(allocate_pool_id(&counter), Err(Error::Overflow));
        assert_eq!(allocate_pool_id(&counter), Err(Error::Overflow));
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn tokens_outlive_a_dropped_pool_without_matching_replacement_pools() {
        let token = {
            let pool = Budget::new(4096).unwrap();
            InputCursor::new(
                &pool,
                SourceId(1),
                SourceCapabilities {
                    length: None,
                    random_access: true,
                },
                16,
            )
            .unwrap()
            .token()
        };
        for _ in 0..256 {
            let pool = Budget::new(4096).unwrap();
            let mut cursor = InputCursor::new(
                &pool,
                SourceId(1),
                SourceCapabilities {
                    length: None,
                    random_access: true,
                },
                16,
            )
            .unwrap();
            let before = cursor.position();
            assert_eq!(
                cursor.provide_inline(SourceId(1), token.clone(), 0, &[1]),
                Err(Error::ForeignPool)
            );
            assert_eq!(cursor.position(), before);
            assert_ne!(cursor.token(), token);
        }
    }
}
