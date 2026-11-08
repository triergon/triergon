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
use triergon_codec::{
    AudioDecoder, Budget, BudgetVec, Charged, DecodeFailure, DownmixLevels, Reservation,
    StorageError,
};

/// Typed decode failure. The decoder never panics on malformed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A valid but unimplemented feature (named).
    Unsupported(&'static str),
    /// Malformed or inconsistent bitstream data (named check).
    InvalidData(&'static str),
    /// Input exceeds a resource bound.
    ResourceLimit(&'static str),
    /// Storage the frame needs does not fit the decoder's budget.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported DTS feature: {s}"),
            DecodeError::InvalidData(s) => write!(f, "invalid DTS data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "DTS resource limit: {s}"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Config {
    amode: u8,
    nch: usize,
    lff: u8,
    filts: bool,
    rate: u32,
}

/// DTS core decoder.
#[derive(Debug)]
pub struct Decoder {
    /// 16-bit big-endian copy of a frame in another packing.
    scratch: BudgetVec<u8>,
    frames: FrameDecoder,
}

/// 32-band synthesis history of one slot.
type History32 = QmfState<1024>;

/// The X96 64-band filter bank, created when a stream first carries X96.
#[derive(Debug)]
struct X96Bank {
    qmf: Qmf<1024>,
    /// One history per coded slot.
    state: BudgetVec<QmfState<2048>>,
}

/// Decoder state of the frame stages, apart from the normalization buffer.
#[derive(Debug)]
pub(crate) struct FrameDecoder {
    sub: SubBuf,
    side: Box<SideInfo>,
    qmf_np: Qmf<512>,
    qmf_pr: Qmf<512>,
    /// One history per coded slot.
    qstate: Box<[History32]>,
    lfe_fir: LfeFir,
    lfe_state: LfeState,
    lfe_dec: [f32; 128],
    /// Time-major synthesis input of one slot (blocks x bands).
    tbuf: BudgetVec<f32>,
    prev: Option<Config>,
    prev_xch: bool,
    prev_x96: bool,
    max_blocks: usize,
    x96buf: Option<SubBuf>,
    x96side: Box<x96::X96Side>,
    x96: Option<Charged<Box<X96Bank>>>,
    lfe2x: Lfe2x,
    lfe48: BudgetVec<f32>,
    /// Pool of the stream-dependent storage above.
    budget: Budget,
    /// Charge of the output frame the last frame sized.
    output_charge: Reservation,
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

impl X96Bank {
    fn new(budget: &Budget) -> Result<Charged<Box<Self>>, StorageError> {
        let state = BudgetVec::try_filled(budget, QmfState::new(), MAX_SLOTS)?;
        Charged::boxed(
            budget,
            Self {
                qmf: Qmf::new(&tables::X96_FIR, QMF64_GAIN * OUT_SCALE),
                state,
            },
        )
    }

    fn reset(&mut self) {
        for q in self.state.iter_mut() {
            q.reset();
        }
    }
}

impl FrameDecoder {
    fn new(budget: &Budget) -> Self {
        Self {
            sub: SubBuf::new(budget, MAX_SLOTS, 32),
            side: Box::new(SideInfo::new()),
            qmf_np: Qmf::new(&tables::QMF_NONPERFECT, QMF_GAIN * OUT_SCALE),
            // FFmpeg's effective PR prototype differs from Annex D.8 (see pr_table.rs).
            qmf_pr: Qmf::new(&pr_table::QMF_PERFECT_REF, QMF_GAIN * OUT_SCALE),
            qstate: (0..MAX_SLOTS).map(|_| QmfState::new()).collect(),
            lfe_fir: LfeFir::new(OUT_SCALE),
            lfe_state: LfeState::default(),
            lfe_dec: [0.0; 128],
            tbuf: BudgetVec::new(budget),
            prev: None,
            prev_xch: false,
            prev_x96: false,
            max_blocks: 0,
            x96buf: None,
            x96side: Box::new(x96::X96Side::new()),
            x96: None,
            lfe2x: Lfe2x::default(),
            lfe48: BudgetVec::new(budget),
            budget: budget.clone(),
            output_charge: Reservation::empty(budget),
        }
    }

    pub(crate) fn reset(&mut self) {
        for q in &mut self.qstate {
            q.reset();
        }
        if let Some(bank) = &mut self.x96 {
            bank.reset();
        }
        self.lfe_state.reset();
        self.lfe2x.reset();
        self.prev = None;
        self.max_blocks = 0;
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
