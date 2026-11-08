#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

pub mod layout;
pub mod mdct;
mod window;

pub use layout::{ChannelLayout, Speaker};
pub use triergon_core::{Budget, BudgetVec, Charged, Error as StorageError, Reservation};
pub use triergon_pixel::{Colour, PictureView, Plane};
pub use window::kbd_window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Ac3,
    Eac3,
    Aac,
    Dts,
    Flac,
    Opus,
    Vorbis,
    /// MPEG-1/2 audio Layer I, II or III.
    MpegAudio,
}

/// Why a decoder refused an access unit: the codec error's `Debug` text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeFailure(pub String);

impl DecodeFailure {
    /// The failure for a codec crate's error.
    pub fn from_error(error: impl core::fmt::Debug) -> Self {
        DecodeFailure(format!("{error:?}"))
    }
}

/// Decoded audio of one access unit: planar `f32` at full scale 1.0.
#[derive(Clone, Debug, Default)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub layout: ChannelLayout,
    pub samples_per_channel: usize,
    /// One plane per speaker of `layout`, in its order, `samples_per_channel` samples each.
    pub planes: Vec<Vec<f32>>,
}

impl AudioFrame {
    /// Drop all samples (keeps allocations).
    pub fn clear(&mut self) {
        for plane in &mut self.planes {
            plane.clear();
        }
        self.samples_per_channel = 0;
    }

    /// Size the frame to `channels` planes of `len` samples each, new samples zero.
    ///
    /// The capacity the frame needs is charged to `charge` before any plane grows,
    /// and the charge then matches the frame's whole capacity, so a decoder's budget
    /// covers the output it fills. A refused charge leaves `charge` unchanged; the
    /// frame may have dropped planes beyond `channels`.
    pub fn try_resize(
        &mut self,
        channels: usize,
        len: usize,
        charge: &mut Reservation,
    ) -> Result<(), StorageError> {
        self.planes.truncate(channels);
        let planes = |count: usize| {
            count
                .checked_mul(size_of::<Vec<f32>>())
                .ok_or(StorageError::Overflow)
        };
        let samples = |count: usize| {
            count
                .checked_mul(size_of::<f32>())
                .ok_or(StorageError::Overflow)
        };
        let mut bytes = planes(self.planes.capacity().max(channels))?;
        for i in 0..channels {
            let capacity = self.planes.get(i).map_or(0, Vec::capacity).max(len);
            bytes = bytes
                .checked_add(samples(capacity)?)
                .ok_or(StorageError::Overflow)?;
        }
        charge.resize_to(bytes)?;
        let mut grew = false;
        if channels > self.planes.len() {
            self.planes
                .try_reserve_exact(channels - self.planes.len())
                .map_err(|_| StorageError::Allocation)?;
            self.planes.resize_with(channels, Vec::new);
            grew = true;
        }
        for plane in &mut self.planes {
            if len > plane.capacity() {
                plane
                    .try_reserve_exact(len - plane.len())
                    .map_err(|_| StorageError::Allocation)?;
                grew = true;
            }
            plane.resize(len, 0.0);
        }
        if grew {
            let mut bytes = planes(self.planes.capacity())?;
            for plane in &self.planes {
                bytes = bytes
                    .checked_add(samples(plane.capacity())?)
                    .ok_or(StorageError::Overflow)?;
            }
            charge.resize_to(bytes)?;
        }
        Ok(())
    }
}

/// Stereo downmix parameters a stream carries (AC-3/E-AC-3 centre/surround/LFE mix
/// levels and Dolby/Pro Logic II surround encoding). The default is the conventional
/// -3 dB centre and surrounds with LFE omitted, used when a stream carries none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownmixLevels {
    pub center: f32,
    pub surround: f32,
    pub lfe: f32,
    pub encoding: SurroundEncoding,
}

impl Default for DownmixLevels {
    fn default() -> Self {
        Self {
            center: core::f32::consts::FRAC_1_SQRT_2,
            surround: core::f32::consts::FRAC_1_SQRT_2,
            lfe: 0.0,
            encoding: SurroundEncoding::None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurroundEncoding {
    None,
    Dolby,
    ProLogicII,
}

/// An Annex-B video decoder with caller timestamps carried through reordering.
pub trait VideoDecoder: Send {
    /// One Annex-B access unit with its caller timestamp.
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeFailure>;
    /// The next picture in output order, if ready. The view borrows the decoder until
    /// the next call; it returns `None` once no picture is ready.
    fn receive(&mut self) -> Option<PictureView<'_>>;
    /// End of stream: every remaining picture becomes receivable.
    fn flush(&mut self);
    /// Discontinuity: drop all decoder state except parameter sets.
    fn reset(&mut self);
}

/// An audio decoder taking one access unit at a time.
pub trait AudioDecoder: Send {
    /// Replace `out` with the output of `au` (`samples_per_channel == 0` when it
    /// produced none). Returns the stream's downmix metadata, if it carries any.
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure>;
    /// Samples at the start of the stream that carry no signal and are discarded unless
    /// the container says otherwise (the Opus pre-skip); 0 for most decoders.
    fn delay(&self) -> usize {
        0
    }
    /// End of stream: replace `out` with any samples the decoder still holds back
    /// (`samples_per_channel == 0` when there are none, as for most decoders).
    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), DecodeFailure> {
        out.clear();
        Ok(())
    }
    /// Discontinuity: drop all signal history.
    fn reset(&mut self);
}
