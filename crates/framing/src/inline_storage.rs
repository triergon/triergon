use triergon_core::{Budget, BudgetVec, Error, Result};

/// A descriptor table that charges its pool only when it needs heap storage.
#[derive(Debug)]
pub(crate) enum InlineStorage<T, const N: usize> {
    Inline { data: [T; N], len: u8 },
    Heap(BudgetVec<T>),
}

impl<T, const N: usize> InlineStorage<T, N> {
    pub(crate) fn new(empty: impl FnMut(usize) -> T) -> Self {
        const { assert!(N <= u8::MAX as usize) };
        Self::Inline {
            data: std::array::from_fn(empty),
            len: 0,
        }
    }

    pub(crate) fn required_heap_bytes(count: usize) -> Result<usize> {
        if count <= N {
            Ok(0)
        } else {
            count.checked_mul(size_of::<T>()).ok_or(Error::Overflow)
        }
    }

    // A fill walk rebuilds these descriptors after the count is known. A
    // failed reservation leaves its empty heap table in place for retry.
    pub(crate) fn reserve(&mut self, pool: &Budget, count: usize) -> Result<()> {
        if count > N {
            if !matches!(self, Self::Heap(_)) {
                *self = Self::Heap(BudgetVec::new(pool));
            }
            if let Self::Heap(values) = self {
                values.try_reserve_exact(count)?;
            }
        }
        Ok(())
    }

    pub(crate) fn clear_inline(&mut self) {
        if let Self::Inline { len, .. } = self {
            *len = 0;
        }
    }

    pub(crate) fn try_push(&mut self, value: T) -> Result<()> {
        match self {
            Self::Inline { data, len } => {
                *data
                    .get_mut(usize::from(*len))
                    .ok_or(Error::Invalid("inline descriptor count"))? = value;
                *len += 1;
                Ok(())
            }
            Self::Heap(values) => values.try_push(value).map_err(|error| error.error),
        }
    }

    // Packet plans fill incrementally, so the first heap reservation must
    // carry the inline entries forward. Commit it only on success.
    pub(crate) fn try_push_spilling<'a>(
        &mut self,
        spill_budget: impl FnOnce() -> (&'a Budget, usize),
        planned: Option<usize>,
        value: T,
    ) -> Result<()>
    where
        T: Copy,
    {
        match self {
            Self::Inline { data, len } if usize::from(*len) < N => {
                data[usize::from(*len)] = value;
                *len += 1;
                Ok(())
            }
            Self::Inline { data, .. } => {
                let (pool, capacity) = spill_budget();
                *self = Self::Heap(Self::spill(pool, planned, capacity, data, value)?);
                Ok(())
            }
            Self::Heap(values) => values.try_push(value).map_err(|error| error.error),
        }
    }

    #[cold]
    #[inline(never)]
    fn spill(
        pool: &Budget,
        planned: Option<usize>,
        max_capacity: usize,
        data: &[T; N],
        value: T,
    ) -> Result<BudgetVec<T>>
    where
        T: Copy,
    {
        let mut values = BudgetVec::new(pool);
        if let Some(planned) = planned {
            // A refused hint falls back to ordinary growth.
            let _ = values.try_reserve_exact(planned);
        }
        let needed = N + 1;
        if values.capacity() < needed {
            let capacity = needed.next_power_of_two().min(max_capacity).max(needed);
            values.try_reserve_exact(capacity).or_else(|error| {
                if capacity != needed
                    && matches!(
                        error,
                        Error::Backpressure { .. } | Error::Limit { .. } | Error::Overflow
                    )
                {
                    values.try_reserve_exact(needed)
                } else {
                    Err(error)
                }
            })?;
        }
        values.try_extend_from_slice(data)?;
        values.try_push(value).map_err(|error| error.error)?;
        Ok(values)
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Inline { len, .. } => usize::from(*len),
            Self::Heap(values) => values.len(),
        }
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        match self {
            Self::Inline { data, len } => &data[..usize::from(*len)],
            Self::Heap(values) => values.as_slice(),
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Self::Inline { data, len } => &mut data[..usize::from(*len)],
            Self::Heap(values) => values.as_mut_slice(),
        }
    }

    pub(crate) fn heap_len(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(values) => values.len(),
        }
    }

    pub(crate) fn heap_capacity(&self) -> usize {
        match self {
            Self::Inline { .. } => 0,
            Self::Heap(values) => values.capacity(),
        }
    }
}
