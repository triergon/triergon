#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(clippy::needless_for_each)]
mod bits;
mod books;
mod dct;
mod error;
mod frame;
mod header;
mod lfe;
mod pack;
#[rustfmt::skip]
mod pr_table;
mod qmf;
mod subframe;
#[doc(hidden)]
#[rustfmt::skip]
pub mod tables;
#[doc(hidden)]
#[rustfmt::skip]
pub mod vq;
mod x96;

pub use pack::Packing;
pub use triergon_codec::{AudioFrame, ChannelLayout, Speaker};

pub use error::DecodeError;

use frame::FrameDecoder;
use triergon_codec::{AudioDecoder, Budget, BudgetVec, DownmixLevels};

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Live storage that depends on the stream: subband rows, the X96 extension's rows
    /// and 64-band filter bank, synthesis scratch, the normalization
    /// buffer of 14-bit and little-endian packings, and the output frame the decoder
    /// fills. The default holds the largest frame the syntax allows (128 blocks, X96,
    /// eight output planes) about three times over.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 2 * 1024 * 1024,
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
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Self {
            scratch: BudgetVec::new(budget),
            frames: FrameDecoder::new(budget),
        }
    }

    /// Drop all history (filter bank, LFE and ADPCM) after a seek or discontinuity.
    pub fn reset(&mut self) {
        self.frames.reset();
    }

    /// Decode exactly one DTS core frame. Replaces the contents of `out`, reusing its
    /// buffers.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        // A failed frame is dropped without touching the synthesis, LFE or ADPCM history,
        // so the next frame continues as if the failed one were absent (FFmpeg's
        // behaviour; checked with the synthetic `bad_dsync5` stream).
        let packing = Packing::detect(au).ok_or(DecodeError::InvalidData("no DTS core sync"))?;
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
        out.clear();
        Decoder::decode(self, au, out)?;
        Ok(None)
    }
    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

/// Conventional name of this speaker set (e.g. `5.1(side)`, as ffprobe prints it), or
/// `None` when it is not one of the named layouts.
pub fn layout_name(layout: &ChannelLayout) -> Option<&'static str> {
    use Speaker::{
        BackCenter, FrontCenter, FrontLeft, FrontRight, LowFrequency, SideLeft, SideRight,
    };
    Some(match layout.speakers() {
        [FrontCenter] => "mono",
        [FrontLeft, FrontRight] => "stereo",
        [FrontLeft, FrontRight, LowFrequency] => "2.1",
        [FrontLeft, FrontRight, FrontCenter] => "3.0",
        [FrontLeft, FrontRight, BackCenter] => "3.0(back)",
        [FrontLeft, FrontRight, FrontCenter, BackCenter] => "4.0",
        [FrontLeft, FrontRight, FrontCenter, LowFrequency, BackCenter] => "4.1",
        [FrontLeft, FrontRight, SideLeft, SideRight] => "quad(side)",
        [FrontLeft, FrontRight, FrontCenter, LowFrequency] => "3.1",
        [FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight] => "5.0(side)",
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            SideLeft,
            SideRight,
        ] => "5.1(side)",
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackCenter,
            SideLeft,
            SideRight,
        ] => "6.1",
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            BackCenter,
            SideLeft,
            SideRight,
        ] => "6.0",
        _ => return None,
    })
}
