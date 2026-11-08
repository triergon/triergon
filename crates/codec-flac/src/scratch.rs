//! The decoder's channel buffers, charged to its budget.
use triergon_codec::{Budget, BudgetVec, StorageError};

/// One block of restored samples per channel the frame needs at once: a plain channel,
/// and a side channel (33-bit for 32-bit samples). Buffers grow to the largest block seen
/// and are never shrunk, so a stream of varying block sizes does not reallocate.
#[derive(Debug)]
pub(crate) struct Scratch {
    plain: BudgetVec<i32>,
    side: BudgetVec<i32>,
    wide_side: BudgetVec<i64>,
}

impl Scratch {
    pub(crate) fn new(budget: &Budget) -> Self {
        Scratch {
            plain: BudgetVec::new(budget),
            side: BudgetVec::new(budget),
            wide_side: BudgetVec::new(budget),
        }
    }

    /// The plain channel, `len` samples.
    pub(crate) fn plain(&mut self, len: usize) -> Result<&mut [i32], StorageError> {
        block(&mut self.plain, len)
    }

    /// The plain channel and a side channel that fits `i32`.
    pub(crate) fn pair(&mut self, len: usize) -> Result<(&mut [i32], &mut [i32]), StorageError> {
        Ok((block(&mut self.plain, len)?, block(&mut self.side, len)?))
    }

    /// The plain channel and a 33-bit side channel.
    pub(crate) fn wide_pair(
        &mut self,
        len: usize,
    ) -> Result<(&mut [i32], &mut [i64]), StorageError> {
        Ok((
            block(&mut self.plain, len)?,
            block(&mut self.wide_side, len)?,
        ))
    }
}

/// The first `len` samples of `buffer`, grown to hold them.
fn block<T: Copy + Default>(
    buffer: &mut BudgetVec<T>,
    len: usize,
) -> Result<&mut [T], StorageError> {
    if buffer.len() < len {
        buffer.try_resize(len, T::default())?;
    }
    Ok(&mut buffer[..len])
}
