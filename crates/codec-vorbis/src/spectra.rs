//! Per-channel vectors of one block, stored back to back.

use crate::storage::Meter;
use triergon_codec::StorageError;

/// One vector of `stride` values per channel.
#[derive(Debug)]
pub(crate) struct Spectra {
    data: Vec<f32>,
    stride: usize,
}

impl Spectra {
    pub(crate) fn new(
        channels: usize,
        stride: usize,
        meter: &mut Meter,
    ) -> Result<Spectra, StorageError> {
        let len = channels.checked_mul(stride).ok_or(StorageError::Overflow)?;
        Ok(Spectra {
            data: meter.vec(len, 0.0)?,
            stride,
        })
    }

    pub(crate) fn channel_mut(&mut self, channel: usize) -> &mut [f32] {
        &mut self.data[channel * self.stride..][..self.stride]
    }

    /// Two different channels at once.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> (&mut [f32], &mut [f32]) {
        debug_assert_ne!(a, b);
        let stride = self.stride;
        let (low, high) = (a.min(b), a.max(b));
        let (head, tail) = self.data.split_at_mut(high * stride);
        let low_slice = &mut head[low * stride..][..stride];
        let high_slice = &mut tail[..stride];
        if a < b {
            (low_slice, high_slice)
        } else {
            (high_slice, low_slice)
        }
    }

    /// Every channel vector, mutably.
    pub(crate) fn channels_mut(&mut self) -> core::slice::ChunksExactMut<'_, f32> {
        self.data.chunks_exact_mut(self.stride)
    }
}
