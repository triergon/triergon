//! Where restored channels go: the float frame of the `AudioDecoder` contract, or the
//! lossless integer samples.
use crate::{DecodeError, FrameInfo};
use triergon_codec::{AudioFrame, ChannelLayout, Reservation, StorageError};

/// A destination for one frame, plane by plane.
pub(crate) trait Output {
    /// Sizes the destination for `frame`; called before any plane is written.
    fn begin(&mut self, frame: &FrameInfo) -> Result<(), DecodeError>;
    /// Writes plane `plane` from the block's samples, right-justified integers.
    fn put(&mut self, plane: usize, samples: impl Iterator<Item = i32>);
    /// Drops what a failed frame wrote.
    fn abandon(&mut self);
}

/// How the float output scales integers: as FFmpeg converts the samples of its FLAC
/// decoder (16-bit for up to 16 bits per sample, else 32-bit, left-justified) to `f32`.
#[derive(Clone, Copy, Debug)]
enum Scale {
    /// `(x << shift) as i16 / 2^15`.
    Short(u32),
    /// `(x << shift) as f32 / 2^31`.
    Long(u32),
}

impl Scale {
    fn of(bits_per_sample: u8) -> Scale {
        let bits = u32::from(bits_per_sample);
        if bits <= 16 {
            Scale::Short(16 - bits)
        } else {
            Scale::Long(32 - bits)
        }
    }
}

/// Planar `f32` at full scale 1.0.
#[derive(Debug)]
pub(crate) struct FloatOutput<'a> {
    frame: &'a mut AudioFrame,
    charge: &'a mut Reservation,
    scale: Scale,
}

impl<'a> FloatOutput<'a> {
    pub(crate) fn new(frame: &'a mut AudioFrame, charge: &'a mut Reservation) -> Self {
        FloatOutput {
            frame,
            charge,
            scale: Scale::Short(0),
        }
    }
}

impl Output for FloatOutput<'_> {
    fn begin(&mut self, frame: &FrameInfo) -> Result<(), DecodeError> {
        let len = frame.samples_per_channel;
        self.frame
            .try_resize(frame.layout.channels(), len, self.charge)?;
        self.frame.sample_rate = frame.sample_rate;
        self.frame.layout = frame.layout;
        self.frame.samples_per_channel = len;
        self.scale = Scale::of(frame.bits_per_sample);
        Ok(())
    }

    #[inline]
    fn put(&mut self, plane: usize, samples: impl Iterator<Item = i32>) {
        let Some(plane) = self.frame.planes.get_mut(plane) else {
            return;
        };
        match self.scale {
            Scale::Short(shift) => fill(plane, samples, |x| {
                f32::from((x << shift) as i16) * (1.0 / 32_768.0)
            }),
            Scale::Long(shift) => fill(plane, samples, |x| {
                (x << shift) as f32 * (1.0 / 2_147_483_648.0)
            }),
        }
    }

    fn abandon(&mut self) {
        self.frame.clear();
    }
}

#[inline(always)]
fn fill(plane: &mut [f32], samples: impl Iterator<Item = i32>, convert: impl Fn(i32) -> f32) {
    plane
        .iter_mut()
        .zip(samples)
        .for_each(|(out, x)| *out = convert(x));
}

/// Decoded samples as integers of the stream's sample size, planar.
#[derive(Clone, Debug, Default)]
pub struct IntegerFrame {
    pub sample_rate: u32,
    pub layout: ChannelLayout,
    /// The value range of every sample: `-2^(bits - 1)` to `2^(bits - 1) - 1`.
    pub bits_per_sample: u8,
    pub samples_per_channel: usize,
    /// Plane after plane, `samples_per_channel` each.
    samples: Vec<i32>,
}

impl IntegerFrame {
    /// The samples of plane `plane` (empty beyond the layout).
    pub fn plane(&self, plane: usize) -> &[i32] {
        let len = self.samples_per_channel;
        plane
            .checked_mul(len)
            .and_then(|start| self.samples.get(start..start + len))
            .unwrap_or_default()
    }

    /// Every plane, in layout order.
    pub fn planes(&self) -> impl Iterator<Item = &[i32]> {
        (0..self.layout.channels()).map(|plane| self.plane(plane))
    }

    /// Sizes the frame to `channels` planes of `len` samples, charging its capacity.
    fn try_resize(
        &mut self,
        channels: usize,
        len: usize,
        charge: &mut Reservation,
    ) -> Result<(), StorageError> {
        let total = channels.checked_mul(len).ok_or(StorageError::Overflow)?;
        let bytes = |count: usize| {
            count
                .checked_mul(size_of::<i32>())
                .ok_or(StorageError::Overflow)
        };
        charge.resize_to(bytes(self.samples.capacity().max(total))?)?;
        if total > self.samples.capacity() {
            self.samples
                .try_reserve_exact(total - self.samples.len())
                .map_err(|_| StorageError::Allocation)?;
            charge.resize_to(bytes(self.samples.capacity())?)?;
        }
        self.samples.resize(total, 0);
        Ok(())
    }

    fn plane_mut(&mut self, plane: usize) -> Option<&mut [i32]> {
        let len = self.samples_per_channel;
        let start = plane.checked_mul(len)?;
        self.samples.get_mut(start..start + len)
    }
}

/// Lossless integer samples.
#[derive(Debug)]
pub(crate) struct IntegerOutput<'a> {
    frame: &'a mut IntegerFrame,
    charge: &'a mut Reservation,
}

impl<'a> IntegerOutput<'a> {
    pub(crate) fn new(frame: &'a mut IntegerFrame, charge: &'a mut Reservation) -> Self {
        IntegerOutput { frame, charge }
    }
}

impl Output for IntegerOutput<'_> {
    fn begin(&mut self, frame: &FrameInfo) -> Result<(), DecodeError> {
        let len = frame.samples_per_channel;
        self.frame
            .try_resize(frame.layout.channels(), len, self.charge)?;
        self.frame.sample_rate = frame.sample_rate;
        self.frame.layout = frame.layout;
        self.frame.bits_per_sample = frame.bits_per_sample;
        self.frame.samples_per_channel = len;
        Ok(())
    }

    #[inline]
    fn put(&mut self, plane: usize, samples: impl Iterator<Item = i32>) {
        if let Some(plane) = self.frame.plane_mut(plane) {
            plane.iter_mut().zip(samples).for_each(|(out, x)| *out = x);
        }
    }

    fn abandon(&mut self) {
        self.frame.samples_per_channel = 0;
        self.frame.samples.clear();
    }
}
