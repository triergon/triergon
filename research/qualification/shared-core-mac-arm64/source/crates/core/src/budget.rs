//! Live capacity reservations shared by storage owners, including caller retention.
use crate::{Error, Result};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct Inner {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
    generation: AtomicUsize,
}

/// One live allocation pool. Clones share accounting rather than multiplying limits.
#[derive(Clone, Debug)]
pub struct Budget(Arc<Inner>);

const CONTROL_BYTES: usize = size_of::<Inner>() + 2 * size_of::<usize>();

impl Budget {
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
                    self.0.peak.fetch_max(total, Ordering::Relaxed);
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
