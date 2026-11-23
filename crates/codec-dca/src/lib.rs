#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod bits;
mod books;
pub mod compat;
mod dct;
mod frame;
mod header;
mod lfe;
mod pack;
mod qmf;
mod subframe;
#[rustfmt::skip]
pub(crate) mod tables;
#[rustfmt::skip]
pub(crate) mod vq;
mod x96;

#[cfg(test)]
extern crate self as triergon_codec_dca;
#[cfg(test)]
mod synthetic;

pub use pack::Packing;
pub use triergon_codec::{AudioFrame, Budget, ChannelLayout, DecodeError, Error, Speaker};

use compat::ffmpeg::Policy;
use frame::FrameDecoder;
use triergon_codec::{AudioDecoder, BudgetVec, DownmixLevels};

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of storage that depends on the stream: subband rows, the X96 extension's
    /// rows and 64-band filter bank, synthesis scratch, the normalization buffer of
    /// 14-bit and little-endian packings, and the output frame the decoder fills. The
    /// default, 2 MiB, holds the largest frame the syntax allows (128 blocks, X96, eight
    /// output planes) about three times over.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 2 << 20,
        }
    }
}

/// DTS core decoder.
#[derive(Debug)]
pub struct Decoder {
    /// 16-bit big-endian copy of a frame in another packing.
    scratch: BudgetVec<u8>,
    frames: FrameDecoder,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with a pool of the default limit.
    pub fn new() -> Self {
        #[expect(
            clippy::expect_used,
            reason = "the default limit is far above a pool's own charge"
        )]
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`. Fails only for a
    /// limit below a pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool, with the standard synthesis prototype.
    pub fn new_in(budget: &Budget) -> Self {
        Self::with_policy_in(Policy::default(), budget)
    }

    /// A decoder with an explicit synthesis prototype, charging the caller's pool.
    pub fn with_policy_in(policy: Policy, budget: &Budget) -> Self {
        Self {
            scratch: BudgetVec::new(budget),
            frames: FrameDecoder::new(budget, policy.prototype()),
        }
    }

    /// Drop all history (filter bank, LFE and ADPCM) after a seek or discontinuity.
    pub fn reset(&mut self) {
        self.frames.reset();
    }

    /// Decodes exactly one DTS core frame.
    /// Replaces `out`; a refusal leaves `samples_per_channel == 0`.
    ///
    /// # Errors
    ///
    /// Refuses malformed core frames, unsupported extensions and frames that exceed
    /// the decoder's storage limit.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let result = self.decode_frame(au, out);
        if result.is_err() {
            out.clear();
        }
        result
    }

    fn decode_frame(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        // A failed frame is dropped without touching the synthesis, LFE or ADPCM history,
        // so the next frame continues as if the failed one were absent (FFmpeg's
        // behaviour; checked with the synthetic `bad_dsync5` stream).
        let packing = Packing::detect(au).ok_or(DecodeError::Invalid("no DTS core sync"))?;
        let frame = match packing {
            Packing::Be16 => au,
            _ => pack::normalize(au, packing, &mut self.scratch)?,
        };
        self.frames.decode_normalized(frame, out)
    }
}

impl AudioDecoder for Decoder {
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        Decoder::decode(self, au, out)?;
        Ok(None)
    }

    fn reset(&mut self) {
        Decoder::reset(self);
    }
}
