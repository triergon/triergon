//! The decoder's channel buffers, charged to its budget.
use crate::DecodeError;
use triergon_codec::{Budget, Reservation, StorageError};

/// One block of restored samples per channel the frame needs at once: a plain channel,
/// and a side channel (33-bit for 32-bit samples). Buffers grow to the largest block seen
/// and are never shrunk, so a stream of varying block sizes does not reallocate.
#[derive(Debug)]
pub(crate) struct Scratch {
    plain: Vec<i32>,
    side: Vec<i32>,
    wide_side: Vec<i64>,
    charge: Reservation,
}

impl Scratch {
    pub(crate) fn new(budget: &Budget) -> Self {
        Scratch {
            plain: Vec::new(),
            side: Vec::new(),
            wide_side: Vec::new(),
            charge: Reservation::empty(budget),
        }
    }

    /// The plain channel, `len` samples.
    pub(crate) fn plain(&mut self, len: usize) -> Result<&mut [i32], DecodeError> {
        self.grow(len, self.side.len(), self.wide_side.len())?;
        Ok(&mut self.plain[..len])
    }

    /// The plain channel and a side channel that fits `i32`.
    pub(crate) fn pair(&mut self, len: usize) -> Result<(&mut [i32], &mut [i32]), DecodeError> {
        self.grow(len, len, self.wide_side.len())?;
        Ok((&mut self.plain[..len], &mut self.side[..len]))
    }

    /// The plain channel and a 33-bit side channel.
    pub(crate) fn wide_pair(
        &mut self,
        len: usize,
    ) -> Result<(&mut [i32], &mut [i64]), DecodeError> {
        self.grow(len, self.side.len(), len)?;
        Ok((&mut self.plain[..len], &mut self.wide_side[..len]))
    }

    /// Grows each buffer to at least the given length, charging the result first.
    fn grow(&mut self, plain: usize, side: usize, wide_side: usize) -> Result<(), DecodeError> {
        let (plain, side, wide_side) = (
            plain.max(self.plain.len()),
            side.max(self.side.len()),
            wide_side.max(self.wide_side.len()),
        );
        if (plain, side, wide_side) == (self.plain.len(), self.side.len(), self.wide_side.len()) {
            return Ok(());
        }
        let bytes = plain
            .checked_add(side)
            .and_then(|n| n.checked_mul(size_of::<i32>()))
            .and_then(|n| n.checked_add(wide_side.checked_mul(size_of::<i64>())?))
            .ok_or(StorageError::Overflow)?;
        self.charge.resize_to(bytes)?;
        for (buffer, len) in [(&mut self.plain, plain), (&mut self.side, side)] {
            buffer
                .try_reserve_exact(len - buffer.len())
                .map_err(|_| StorageError::Allocation)?;
            buffer.resize(len, 0);
        }
        self.wide_side
            .try_reserve_exact(wide_side - self.wide_side.len())
            .map_err(|_| StorageError::Allocation)?;
        self.wide_side.resize(wide_side, 0);
        Ok(())
    }
}
