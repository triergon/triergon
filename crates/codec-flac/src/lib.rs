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

mod error;
mod frame;
mod layout;
mod output;
mod predict;
mod residual;
mod sample;
mod scratch;
mod stereo;
mod stream;
mod subframe;

pub use error::DecodeError;
pub use output::IntegerFrame;
pub use triergon_codec::{AudioFrame, Budget, ChannelLayout, StorageError};

use frame::FrameDecoder;
use output::{FloatOutput, IntegerOutput};
use stream::Stream;
use triergon_codec::{AudioDecoder, DecodeFailure, DownmixLevels, Reservation};

/// Bounds of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Heap bytes the decoder may hold: its channel buffers and the output frames it
    /// fills. The default holds the largest frame RFC 9639 allows (65536 samples of eight
    /// channels) in both outputs, the buffers of a 32-bit stereo frame, and a copy of the
    /// largest buffer while it grows.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 7 << 20,
        }
    }
}

/// What to do with the CRC-16 that ends every frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CrcPolicy {
    /// Do not compute it; a damaged frame decodes to whatever it holds. FFmpeg's default.
    #[default]
    Ignore,
    /// Refuse a frame whose CRC-16 does not match with [`DecodeError::CrcMismatch`].
    Reject,
}

/// The format of one decoded frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub sample_rate: u32,
    pub bits_per_sample: u8,
    pub layout: ChannelLayout,
    pub samples_per_channel: usize,
}

/// A FLAC decoder: one frame per access unit.
#[derive(Debug)]
pub struct Decoder {
    frames: FrameDecoder,
    /// Pool of the stream-sized storage, which [`IntegerFrame`]s charge as well.
    budget: Budget,
    /// Charge of the float frame the last access unit sized.
    output_charge: Reservation,
}

impl Decoder {
    /// A decoder for the canonical configuration `configuration` (`fLaC` and the
    /// metadata blocks, STREAMINFO first), with a pool of the default limit.
    ///
    /// Empty `configuration` decodes a stream picked up at a frame header, as FFmpeg
    /// does without extradata: each frame must code its sample rate and size.
    pub fn new(configuration: &[u8]) -> Result<Decoder, DecodeError> {
        Decoder::with_limits(configuration, Limits::default())
    }

    /// As [`Decoder::new`], with its own pool of `limits.live_storage_bytes`.
    pub fn with_limits(configuration: &[u8], limits: Limits) -> Result<Decoder, DecodeError> {
        Decoder::new_in(configuration, &Budget::new(limits.live_storage_bytes)?)
    }

    /// As [`Decoder::new`], charging the caller's pool.
    pub fn new_in(configuration: &[u8], budget: &Budget) -> Result<Decoder, DecodeError> {
        Ok(Decoder {
            frames: FrameDecoder::new(Stream::from_configuration(configuration)?, budget),
            budget: budget.clone(),
            output_charge: Reservation::empty(budget),
        })
    }

    /// The decoder with `policy` for frame CRCs.
    #[must_use]
    pub fn with_crc_policy(mut self, policy: CrcPolicy) -> Decoder {
        self.frames.crc = policy;
        self
    }

    /// Replaces `out` with the frame `au` holds as planar `f32`, scaled as FFmpeg's FLAC
    /// decoder output converts to `f32`. On failure `out` is left empty.
    pub fn decode_float(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<FrameInfo, DecodeError> {
        let mut output = FloatOutput::new(out, &mut self.output_charge);
        self.frames.decode(au, &mut output)
    }

    /// An empty frame for [`Decoder::decode_integer`], charged to this decoder's pool.
    pub fn integer_frame(&self) -> IntegerFrame {
        IntegerFrame::new_in(&self.budget)
    }

    /// Replaces `out` with the frame `au` holds as the lossless integer samples. On
    /// failure `out` is left empty.
    pub fn decode_integer(
        &mut self,
        au: &[u8],
        out: &mut IntegerFrame,
    ) -> Result<FrameInfo, DecodeError> {
        let mut output = IntegerOutput::new(out);
        self.frames.decode(au, &mut output)
    }
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        self.decode_float(au, out)
            .map(|_| None)
            .map_err(DecodeFailure::from_error)
    }

    /// FLAC frames are independent: there is no signal history to drop.
    fn reset(&mut self) {}
}
