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

mod analysis;
mod bits;
mod block;
mod dct;
mod error;
mod macroblock;
mod motion;
mod picture;
mod planes;
mod rate;
mod syntax;
mod tables;

use analysis::Decision;
use bits::BitWriter;
use block::Quantizer;
pub use error::{Error, Result};
use motion::Mv;
use planes::Planes;
use rate::RateControl;
pub use triergon_core::{Budget, Error as StorageError};
use triergon_core::{BudgetVec, Reservation};
use triergon_pixel::{Geometry, Picture, PictureView};

/// The parameters of one coded sequence. [`Encoder::new`] refuses values outside the
/// ranges given here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Size of every input picture, and of the coded pictures.
    pub geometry: Geometry,
    /// Average rate the GOP allocation aims for, in bit/s; nonzero.
    pub bit_rate: u32,
    /// Rate at which the VBV buffer fills, in bit/s, and the sequence header's
    /// bit_rate, rounded up to 400 bit/s units; 1 to 104,857,200.
    pub max_rate: u32,
    /// VBV buffer size in bits, signalled rounded up to 16,384-bit units; 1 to
    /// 16,760,832 (1023 units).
    pub vbv_bits: u32,
    /// Intra period: an I picture at least every `gop` pictures; nonzero.
    pub gop: u32,
}

/// Storage limits of one encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Live storage sized from the configuration: the reconstructed and reference
    /// pictures, the previous source luma, the macroblock and motion arrays, the bit
    /// writer and the output buffer at the size bound of one picture, and the first
    /// I picture's trial pass. The default holds the largest geometry (4094x4094) at
    /// the largest VBV buffer the sequence header signals.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    /// 96 MiB of live storage.
    fn default() -> Self {
        Limits {
            live_storage_bytes: 96 * 1024 * 1024,
        }
    }
}

const FPS: f64 = 30.0;

/// Bits one macroblock can take: an 11-bit address increment, a 6-bit type, a 5-bit
/// quantiser, two 14-bit vector components, a 9-bit coded_block_pattern and six
/// blocks of DC size and differential (8 + 8), 64 escaped 28-bit coefficients and
/// end_of_block. Address escapes are counted with `GUARDED_MB_BITS`.
const MB_BITS: usize = 11 + 6 + 5 + 2 * 14 + 9 + 6 * (8 + 8 + 64 * 28 + 2);
/// Bits one macroblock can take once the VBV guard of `encode_row` holds: DC-only
/// intra (at most 116) or uncoded inter (at most 42), plus its share of address
/// escapes (11 bits per 33 macroblocks).
const GUARDED_MB_BITS: usize = 128;
/// Sequence, GOP, picture and slice headers with their alignment.
const HEADER_BITS: usize = 512;

/// What one [`Encoder::encode`] call coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedPicture {
    /// An I picture, preceded by a sequence header and a GOP header: a point a
    /// decoder can start at.
    pub key: bool,
    /// Position of the picture in the sequence, from 0.
    pub index: u64,
    /// Size of the coded picture in bits: eight times the bytes written to `out`.
    pub bits: usize,
    /// Mean quantizer_scale of the picture's macroblocks, rounded; 2 to 31.
    pub qscale_mean: u32,
}

/// An MPEG-1 video encoder for one sequence of pictures of one geometry.
///
/// Each successful [`encode`](Self::encode) codes the next picture as an I or P
/// picture and returns its complete bytes, with no reordering delay. The storage the
/// configuration needs is charged to the encoder's [`Budget`] when it is built; only
/// the motion search's decimated planes and the caller's output are charged later,
/// before a picture is coded.
#[derive(Debug)]
pub struct Encoder {
    cfg: EncoderConfig,
    reference: Planes,
    recon: Planes,
    writer: BitWriter,
    /// Writer of the first I picture's trial pass, dropped once it has run.
    probe: Option<BitWriter>,
    quant: Box<Quantizer>,
    rc: RateControl,
    analysis: BudgetVec<Decision>,
    row_activity: BudgetVec<f64>,
    previous_mvs: BudgetVec<Mv>,
    /// Current and reference luma for motion search, one half each.
    coarse_planes: BudgetVec<u8>,
    /// Luma of the previous source picture, for scene-change detection.
    previous_luma: BudgetVec<u8>,
    /// Chroma (Cb, then Cr) of the previous source picture: with the luma, it
    /// tells a repeated picture.
    previous_chroma: BudgetVec<u8>,
    /// Per macroblock, the quantiser at which the previous picture skipped it
    /// after its residual quantised to zero; 0 where it did not.
    skipped_at: BudgetVec<u8>,
    /// Macroblocks skipped again from `skipped_at` without coding a residual.
    #[cfg(test)]
    reused_skips: usize,
    /// Macroblocks identical to the previous source (scene-score scratch).
    unchanged: BudgetVec<u32>,
    /// Per-block quantised levels of the macroblock being coded; entries are
    /// only meaningful where the block's non-zero mask says so.
    levels: [[i32; 64]; 6],
    /// Index of the next picture and of the first picture of the current GOP.
    index: u64,
    gop_start: u64,
    /// Charge of the caller's output buffer: its capacity, at least `picture_bound`.
    output_charge: Reservation,
    /// Most bytes one picture can take.
    picture_bound: usize,
}

impl Encoder {
    /// An encoder with its own pool of the default [`Limits`]. Fails as
    /// [`new_in`](Self::new_in) does.
    pub fn new(cfg: EncoderConfig) -> Result<Encoder> {
        Encoder::with_limits(cfg, Limits::default())
    }

    /// An encoder with its own pool of `limits.live_storage_bytes`. Fails as
    /// [`new_in`](Self::new_in) does, and refuses the configuration before creating
    /// the pool.
    pub fn with_limits(cfg: EncoderConfig, limits: Limits) -> Result<Encoder> {
        validate(&cfg)?;
        Encoder::new_in(cfg, &Budget::new(limits.live_storage_bytes)?)
    }

    /// An encoder charging the caller's pool. Fails with [`Error::BitRate`],
    /// [`Error::VbvBuffer`] or [`Error::Gop`] for a configuration outside the ranges of
    /// [`EncoderConfig`], and with [`Error::Budget`] when its storage does not fit.
    pub fn new_in(cfg: EncoderConfig, budget: &Budget) -> Result<Encoder> {
        validate(&cfg)?;
        let g = cfg.geometry;
        let mbs = g.macroblock_width() * g.macroblock_height();
        let picture_bound = picture_bound(&cfg)?;
        Ok(Encoder {
            cfg,
            reference: Planes::try_new(budget, g)?,
            recon: Planes::try_new(budget, g)?,
            writer: BitWriter::try_new(budget, picture_bound)?,
            // One row of full macroblocks: the bound of the first I picture's trial pass.
            probe: Some(BitWriter::try_new(
                budget,
                (g.macroblock_width() * MB_BITS).div_ceil(8) + 8,
            )?),
            quant: Quantizer::new(),
            rc: RateControl::new(cfg.bit_rate, cfg.max_rate, cfg.vbv_bits, cfg.gop, FPS),
            analysis: BudgetVec::try_filled(budget, Decision::default(), mbs)?,
            row_activity: BudgetVec::try_filled(budget, 0.0, g.macroblock_height())?,
            previous_mvs: BudgetVec::try_filled(budget, Mv::ZERO, mbs)?,
            coarse_planes: BudgetVec::new(budget),
            previous_luma: BudgetVec::try_filled(
                budget,
                0,
                g.luma_stride() * g.macroblock_height() * 16,
            )?,
            previous_chroma: BudgetVec::try_filled(
                budget,
                0,
                2 * g.chroma_stride() * g.macroblock_height() * 8,
            )?,
            skipped_at: BudgetVec::try_filled(budget, 0, mbs)?,
            #[cfg(test)]
            reused_skips: 0,
            unchanged: BudgetVec::try_filled(budget, 0, mbs)?,
            levels: [[0; 64]; 6],
            index: 0,
            gop_start: 0,
            output_charge: Reservation::empty(budget),
            picture_bound,
        })
    }

    /// The configuration the encoder was built with.
    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Bits by which pictures so far exceeded the modelled VBV occupancy, summed;
    /// 0 while every picture fitted.
    pub fn vbv_underflow_bits(&self) -> f64 {
        self.rc.underflow_bits
    }

    /// What a decoder reconstructs from the most recent picture, over the coded
    /// planes; zero luma and neutral chroma before the first picture.
    pub fn reconstruction(&self) -> PictureView<'_> {
        self.reference.view()
    }

    /// Code `input` as the next picture and replace the contents of `out` with its
    /// bytes. The input must have the configured geometry and its coded padding
    /// filled ([`Picture::pad_edges`]).
    ///
    /// A geometry mismatch is [`Error::PictureGeometry`]; storage refused before
    /// coding is [`Error::Budget`]. Either error codes nothing and leaves `out` as it
    /// was: release space and retry the same input at the same picture index.
    pub fn encode(&mut self, input: &Picture, out: &mut Vec<u8>) -> Result<EncodedPicture> {
        if input.geometry() != self.cfg.geometry {
            return Err(Error::PictureGeometry {
                expected: self.cfg.geometry,
                actual: input.geometry(),
            });
        }
        // The caller's buffer is a plain `Vec`, so its charge is held here: it takes
        // at most `picture_bound`, and `take_into` grows it to exactly a picture.
        self.output_charge
            .resize_to(out.capacity().max(self.picture_bound))?;
        self.encode_picture(input, out)
    }
}

fn validate(cfg: &EncoderConfig) -> Result<()> {
    if cfg.gop == 0 {
        return Err(Error::Gop(cfg.gop));
    }
    if cfg.bit_rate == 0 {
        return Err(Error::BitRate(cfg.bit_rate));
    }
    if cfg.max_rate == 0 || cfg.max_rate.div_ceil(400) > 0x3FFFF {
        return Err(Error::BitRate(cfg.max_rate));
    }
    if cfg.vbv_bits == 0 || cfg.vbv_bits.div_ceil(16384) > 1023 {
        return Err(Error::VbvBuffer(cfg.vbv_bits));
    }
    Ok(())
}

/// Most bytes one picture can take. Rate control never plans beyond the VBV
/// occupancy (at most `vbv_bits`), and once a picture comes within 8192 bits of
/// it `encode_row` codes only guarded macroblocks, so one full macroblock and
/// guarded ones are all that can follow.
fn picture_bound(cfg: &EncoderConfig) -> Result<usize> {
    let g = cfg.geometry;
    // Geometry caps the macroblock count at 65 536.
    let mbs = g.macroblock_width() * g.macroblock_height();
    let bits = (cfg.vbv_bits as usize)
        .checked_add(HEADER_BITS + MB_BITS + mbs * GUARDED_MB_BITS)
        .ok_or(StorageError::Overflow)?;
    Ok(bits.div_ceil(8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest configuration fits the default with room for what encoding adds:
    /// the coarse planes (2 MiB), the caller's output at the picture bound (3 MiB)
    /// and the trial pass.
    #[test]
    fn the_default_limit_holds_the_largest_configuration() {
        let cfg = EncoderConfig {
            geometry: Geometry::new(4094, 4094).unwrap(),
            bit_rate: 100_000_000,
            max_rate: 100_000_000,
            vbv_bits: 1023 * 16384,
            gop: 30,
        };
        let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
        let _encoder = Encoder::new_in(cfg, &budget).unwrap();
        assert!(budget.available() > 8 << 20, "{}", budget.used());
        assert!(budget.used() > budget.limit() * 2 / 3, "{}", budget.used());
    }
}
