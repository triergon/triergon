//! DTS Coherent Acoustics core decoder (ETSI TS 102 114). Pure safe Rust. Not yet qualified.
//!
//! Input: one complete DTS core frame per call (16-bit or 14-bit word packing, big or
//! little endian), optionally followed by the core-embedded XCh / X96 extensions that
//! live inside the frame. Output: planar `f32` at full scale 1.0 in FFmpeg's native
//! channel order for the stream (see [`Speaker`] and [`ChannelLayout`]).
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

use lfe::{Lfe2x, LfeFir, LfeState};
use qmf::{Qmf, QmfState};
use subframe::{SideInfo, SubBuf};

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

/// Loudspeaker position of an output plane. The discriminant is FFmpeg's channel bit
/// index (`AV_CHAN_*`), and planes are emitted in ascending discriminant order, which is
/// FFmpeg's native order for the layouts this decoder produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Speaker {
    FrontLeft = 0,
    FrontRight = 1,
    FrontCenter = 2,
    LowFrequency = 3,
    BackLeft = 4,
    BackRight = 5,
    FrontLeftOfCenter = 6,
    FrontRightOfCenter = 7,
    BackCenter = 8,
    SideLeft = 9,
    SideRight = 10,
}

impl Speaker {
    /// FFmpeg's short channel name.
    pub fn name(self) -> &'static str {
        match self {
            Speaker::FrontLeft => "FL",
            Speaker::FrontRight => "FR",
            Speaker::FrontCenter => "FC",
            Speaker::LowFrequency => "LFE",
            Speaker::BackLeft => "BL",
            Speaker::BackRight => "BR",
            Speaker::FrontLeftOfCenter => "FLC",
            Speaker::FrontRightOfCenter => "FRC",
            Speaker::BackCenter => "BC",
            Speaker::SideLeft => "SL",
            Speaker::SideRight => "SR",
        }
    }
}

/// Speaker position of each output plane, in plane order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelLayout {
    speakers: [Speaker; 8],
    count: u8,
}

impl Default for ChannelLayout {
    fn default() -> Self {
        Self {
            speakers: [Speaker::FrontCenter; 8],
            count: 0,
        }
    }
}

impl ChannelLayout {
    /// Speakers in plane order.
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers[..self.count as usize]
    }

    /// Number of planes.
    pub fn channels(&self) -> usize {
        self.count as usize
    }

    /// Conventional name of this speaker set (e.g. `5.1(side)`, as ffprobe prints it), or `None` when it is
    /// not one of the named layouts.
    pub fn layout_name(&self) -> Option<&'static str> {
        use Speaker::*;
        Some(match self.speakers() {
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
}

/// Decoded audio of one frame.
#[derive(Clone, Debug, Default)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub layout: ChannelLayout,
    pub samples_per_channel: usize,
    /// One plane per channel in `layout` order; each holds `samples_per_channel` samples.
    pub planes: Vec<Vec<f32>>,
}

/// Maximum coded primary channels (core 5 + extension channels).
const MAX_SLOTS: usize = 8;
/// Output scale: core PCM is 24-bit integer domain.
const OUT_SCALE: f64 = 1.0 / 8_388_608.0;
/// Synthesis gain applied to the C.3.6 prototype: the 0.25 of the modulation stage times
/// 2^7.5. C.3.6 leaves its output `rScale` unspecified; 2^7.5 is the black-box
/// calibration against FFmpeg's float output (`research/d4/dca/STATUS.md`).
const QMF_GAIN: f64 = 0.25 * 128.0 * std::f64::consts::SQRT_2;
/// X96 64-band synthesis gain (black-box calibration, see STATUS.md).
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
        // behaviour; synthetic `bad_dsync5` check in STATUS.md).
        res
    }
}
