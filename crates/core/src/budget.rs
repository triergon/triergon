//! Live capacity reservations shared by storage owners, including caller retention.
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

/// One live allocation pool. Clones share accounting rather than multiplying limits.
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
    /// Fixed tracked charge for this pool's shared control allocation. Cloning
    /// the pool does not create or charge another control allocation.
    pub const fn control_storage_bytes() -> usize {
        CONTROL_BYTES
    }
    /// Includes the pool's own fixed Arc/control storage. Allocator overhead is
    /// measured separately; these counters track requested/retained capacity.
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
    pub fn limit(&self) -> usize {
        self.0.limit
    }
    pub fn used(&self) -> usize {
        self.0.used.load(Ordering::Relaxed)
    }
    /// High-water charged capacity, including pending reservations, not process RSS.
    pub fn peak(&self) -> usize {
        self.0.peak.load(Ordering::Relaxed)
    }
    pub fn available(&self) -> usize {
        self.limit() - self.used()
    }
    pub fn same_pool(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn identity(&self) -> PoolId {
        self.0.id
    }
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

/// Linear charge. Keep it inside the allocation owner until that allocation drops.
/// It intentionally cannot be cloned independently of the storage it protects.
#[derive(Debug)]
pub struct Reservation {
    pub(crate) budget: Budget,
    bytes: usize,
}

impl Reservation {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub(crate) fn empty(budget: &Budget) -> Self {
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
    /// Claims `extra` more bytes, with the pool's own limit and pressure checks.
    pub(crate) fn grow_by(&mut self, extra: usize) -> Result<()> {
        self.budget.claim(extra)?;
        // Both terms are bounded by the pool limit after a successful claim.
        self.bytes += extra;
        Ok(())
    }
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
