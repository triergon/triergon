//! Per-channel vectors of one block, stored back to back.

use triergon_codec::{Budget, BudgetVec, StorageError};

/// One vector of `len` values per channel.
///
/// Vectors start [`PAD`] values more than `len` apart: with power-of-two lengths they
/// would otherwise sit a multiple of 4 KiB apart, and loads from one channel would
/// falsely wait on stores to another (4K aliasing), as in inverse coupling.
#[derive(Debug)]
pub(crate) struct Spectra {
    data: BudgetVec<f32>,
    len: usize,
    stride: usize,
}

/// Values between the end of one channel's vector and the start of the next.
const PAD: usize = 16;

impl Spectra {
    pub(crate) fn new(
        channels: usize,
        len: usize,
        budget: &Budget,
    ) -> Result<Spectra, StorageError> {
        let stride = len + PAD;
        let size = channels.checked_mul(stride).ok_or(StorageError::Overflow)?;
        Ok(Spectra {
            data: BudgetVec::try_filled(budget, 0.0, size)?,
            len,
            stride,
        })
    }

    pub(crate) fn channel_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.data[channel * self.stride..][..self.len]
    }

    /// Two different channels at once.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> (&mut [f32], &mut [f32]) {
        debug_assert_ne!(a, b);
        let (stride, len) = (self.stride, self.len);
        let (low, high) = (a.min(b), a.max(b));
        let (head, tail) = self.data.split_at_mut(high * stride);
        let low_slice = &mut head[low * stride..][..len];
        let high_slice = &mut tail[..len];
        if a < b {
            (low_slice, high_slice)
        } else {
            (high_slice, low_slice)
        }
    }

    /// The vectors of `C` different channels at once; `None` if a channel repeats.
    pub(crate) fn disjoint_mut<const C: usize>(
        &mut self,
        channels: [usize; C],
    ) -> Option<[&mut [f32]; C]> {
        let ranges =
            channels.map(|channel| channel * self.stride..channel * self.stride + self.len);
        self.data.get_disjoint_mut(ranges).ok()
    }

    /// Every channel vector, mutably.
    pub(crate) fn channels_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        let len = self.len;
        self.data
            .chunks_exact_mut(self.stride)
            .map(move |channel| &mut channel[..len])
    }
}
