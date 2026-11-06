//! DTS Coherent Acoustics core decoder (ETSI TS 102 114). Pure safe Rust.
//!
//! Input: one complete DTS core frame per call (16-bit or 14-bit word packing, big or
//! little endian), optionally followed by the core-embedded XCh / X96 extensions that
//! live inside the frame. Output: planar `f32` at full scale 1.0 in FFmpeg's native
//! channel order for the stream (see [`triergon_codec::layout`]).
#![forbid(unsafe_code)]
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

mod bits;
mod books;
mod dct;
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

use lfe::{Lfe2x, LfeFir, LfeState};
use qmf::{Qmf, QmfState};
use subframe::{SideInfo, SubBuf};
use triergon_codec::{AudioDecoder, DecodeFailure, DownmixLevels};

/// Typed decode failure. The decoder never panics on malformed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A valid but unimplemented feature (named).
    Unsupported(&'static str),
    /// Malformed or inconsistent bitstream data (named check).
    InvalidData(&'static str),
    /// Input exceeds a resource bound.
    ResourceLimit(&'static str),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported DTS feature: {s}"),
            DecodeError::InvalidData(s) => write!(f, "invalid DTS data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "DTS resource limit: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Maximum coded primary channels (core 5 + extension channels).
const MAX_SLOTS: usize = 8;
/// Output scale: core PCM is 24-bit integer domain.
const OUT_SCALE: f64 = 1.0 / 8_388_608.0;
/// Synthesis gain applied to the C.3.6 prototype: the 0.25 of the modulation stage times
/// 2^7.5. C.3.6 leaves its output `rScale` unspecified; 2^7.5 is the black-box
/// calibration against FFmpeg's float output.
const QMF_GAIN: f64 = 0.25 * 128.0 * std::f64::consts::SQRT_2;
/// X96 64-band synthesis gain (black-box calibration).
const QMF64_GAIN: f64 = 0.25 * 256.0 * std::f64::consts::SQRT_2;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Config {
    amode: u8,
    nch: usize,
    lff: u8,
    filts: bool,
    rate: u32,
}

/// DTS core decoder.
pub struct Decoder {
    scratch: Vec<u8>,
    sub: SubBuf,
    side: Box<SideInfo>,
    qmf_np: Qmf,
    qmf_pr: Qmf,
    qstate: Vec<QmfState>,
    lfe_fir: LfeFir,
    lfe_state: LfeState,
    lfe_dec: [f32; 128],
    /// Time-major synthesis input of one slot (blocks x bands).
    tbuf: Vec<f32>,
    prev: Option<Config>,
    prev_xch: bool,
    prev_x96: bool,
    max_blocks: usize,
    x96buf: Option<SubBuf>,
    x96side: Box<x96::X96Side>,
    qmf64: Option<Qmf>,
    q64state: Vec<QmfState>,
    lfe2x: Lfe2x,
    lfe48: Vec<f32>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            scratch: Vec::new(),
            sub: SubBuf::new(MAX_SLOTS, 32),
            side: Box::new(SideInfo::new()),
            qmf_np: Qmf::new(&tables::QMF_NONPERFECT, QMF_GAIN * OUT_SCALE),
            // FFmpeg's effective PR prototype differs from Annex D.8 (see pr_table.rs).
            qmf_pr: Qmf::new(&pr_table::QMF_PERFECT_REF, QMF_GAIN * OUT_SCALE),
            qstate: (0..MAX_SLOTS).map(|_| QmfState::new(32)).collect(),
            lfe_fir: LfeFir::new(OUT_SCALE),
            lfe_state: LfeState::default(),
            lfe_dec: [0.0; 128],
            tbuf: Vec::new(),
            prev: None,
            prev_xch: false,
            prev_x96: false,
            max_blocks: 0,
            x96buf: None,
            x96side: Box::new(x96::X96Side::new()),
            qmf64: None,
            q64state: Vec::new(),
            lfe2x: Lfe2x::default(),
            lfe48: Vec::new(),
        }
    }

    /// Drop all history (filter bank, LFE and ADPCM) after a seek or discontinuity.
    pub fn reset(&mut self) {
        for q in &mut self.qstate {
            q.reset();
        }
        for q in &mut self.q64state {
            q.reset();
        }
        self.lfe_state.reset();
        self.lfe2x.reset();
        self.prev = None;
        self.max_blocks = 0;
    }

    /// Decode exactly one DTS core frame. Replaces the contents of `out`, reusing its
    /// buffers.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let packing = Packing::detect(au).ok_or(DecodeError::InvalidData("no DTS core sync"))?;
        let mut scratch = core::mem::take(&mut self.scratch);
        let res = if packing == Packing::Be16 {
            self.decode_normalized(au, out)
        } else {
            pack::normalize(au, packing, &mut scratch);
            self.decode_normalized(&scratch, out)
        };
        self.scratch = scratch;
        // A failed frame is dropped without touching the synthesis, LFE or ADPCM history,
        // so the next frame continues as if the failed one were absent (FFmpeg's
        // behaviour; checked with the synthetic `bad_dsync5` stream).
        res
    }
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        out.clear();
        Decoder::decode(self, au, out).map_err(DecodeFailure::from_error)?;
        Ok(None)
    }
    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

/// Conventional name of this speaker set (e.g. `5.1(side)`, as ffprobe prints it), or
/// `None` when it is not one of the named layouts.
pub fn layout_name(layout: &ChannelLayout) -> Option<&'static str> {
    use Speaker::*;
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
