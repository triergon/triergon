//! MPEG-1 video (ISO/IEC 11172-2) encoder: I and P pictures, half-sample
//! forward motion compensation, one slice per macroblock row, closed GOPs
//! with a sequence header before every I picture, and VBV-constrained rate
//! control. Each `encode` call returns that picture's complete bytes; there
//! is no reordering delay (contract difference D1).

#![forbid(unsafe_code)]

mod bits;
mod block;
mod dct;
mod error;
#[doc(hidden)]
pub mod motion;
mod rate;
mod tables;

use bits::BitWriter;
use block::Quantizer;
pub use error::{Error, Result};
use motion::{Bounds, Coarse, Mv, Search};
use rate::{Kind, RateControl};
use tables::*;
use triergon_pixel::{Geometry, Picture};

/// Encoder parameters. `Session` derives them from the Streem rate rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    pub geometry: Geometry,
    /// Average target in bits per second.
    pub bit_rate: u32,
    /// VBV fill rate and sequence-header bit_rate, bits per second.
    pub max_rate: u32,
    /// VBV buffer size in bits.
    pub vbv_bits: u32,
    /// Intra period in pictures.
    pub gop: u32,
}

/// Picture rate 30 Hz: frame_rate_code 5 in Table 2-D.4.
const FRAME_RATE_CODE: u32 = 5;
const FPS: f64 = 30.0;
/// Search window in half-samples (±64 luma samples).
const SEARCH_RANGE: i32 = 128;
const MAX_F_CODE: u32 = 4;
/// A P macroblock is intra coded when its deviation from its own mean beats
/// the motion-compensated SAD by this margin (intra blocks cost more bits).
const INTRA_MARGIN: u32 = 512;

/// Facts about one encoded picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodedPicture {
    pub key: bool,
    /// Picture index within the session (0-based).
    pub index: u64,
    pub bits: usize,
    pub qscale_mean: u32,
}

#[derive(Clone, Copy, Default)]
struct Analysis {
    mv: Mv,
    sad: u32,
    intra: bool,
}

pub struct Encoder {
    cfg: EncoderConfig,
    reference: Picture,
    recon: Picture,
    writer: BitWriter,
    quant: Box<Quantizer>,
    rc: RateControl,
    analysis: Vec<Analysis>,
    row_activity: Vec<f64>,
    previous_mvs: Vec<Mv>,
    coarse_cur: Vec<u8>,
    coarse_ref: Vec<u8>,
    /// Luma of the previous source picture, for scene-change detection.
    previous_luma: Vec<u8>,
    /// Macroblocks identical to the previous source (scene-score scratch).
    unchanged: Vec<u32>,
    /// Per-block quantised levels of the macroblock being coded; entries are
    /// only meaningful where the block's non-zero mask says so.
    levels: [[i32; 64]; 6],
    index: u64,
    gop_start: u64,
}

/// Per-slice state reset at every slice start. Each picture is one slice
/// (ISO/IEC 11172-2 lets a slice span macroblock rows), so DC and vector
/// predictors run across row ends; per-row quantiser changes travel in
/// macroblock_quant fields instead of slice headers.
struct SliceState {
    /// quantizer_scale currently in force at the decoder.
    q: u32,
    /// quantizer_scale wanted for the current row; sent with the next
    /// macroblock that carries coefficients.
    target_q: u32,
    dc_pred: [i32; 3],
    pmv: Mv,
    last_intra: Option<usize>,
    skipped: usize,
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Encoder> {
        if cfg.gop == 0 {
            return Err(Error::Gop(0));
        }
        if cfg.max_rate == 0
            || cfg.max_rate.div_ceil(400) > 0x3FFFF
            || cfg.bit_rate == 0
            || cfg.vbv_bits == 0
        {
            return Err(Error::BitRate(cfg.bit_rate as u64));
        }
        let g = cfg.geometry;
        let mbs = g.macroblock_width() * g.macroblock_height();
        Ok(Encoder {
            cfg,
            reference: Picture::new(g),
            recon: Picture::new(g),
            writer: BitWriter::with_capacity(cfg.vbv_bits as usize / 8 + 4096),
            quant: Quantizer::new(),
            rc: RateControl::new(cfg.bit_rate, cfg.max_rate, cfg.vbv_bits, cfg.gop, FPS),
            analysis: vec![Analysis::default(); mbs],
            row_activity: vec![0.0; g.macroblock_height()],
            previous_mvs: vec![Mv::ZERO; mbs],
            coarse_cur: Vec::new(),
            coarse_ref: Vec::new(),
            previous_luma: vec![0; g.luma_stride() * g.macroblock_height() * 16],
            unchanged: Vec::with_capacity(mbs),
            levels: [[0; 64]; 6],
            index: 0,
            gop_start: 0,
        })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    /// Total VBV underflow observed by the rate controller (bits).
    pub fn vbv_underflow_bits(&self) -> f64 {
        self.rc.underflow_bits
    }

    /// The reconstruction of the most recently encoded picture.
    pub fn reconstruction(&self) -> &Picture {
        &self.reference
    }

    /// Encode `input` (same geometry, padded) into `out` (cleared first).
    pub fn encode(&mut self, input: &Picture, out: &mut Vec<u8>) -> Result<EncodedPicture> {
        if input.geometry() != self.cfg.geometry {
            return Err(Error::PictureGeometry {
                expected: self.cfg.geometry,
                actual: input.geometry(),
            });
        }
        Ok(self.encode_validated(input, out))
    }
    fn encode_validated(&mut self, input: &Picture, out: &mut Vec<u8>) -> EncodedPicture {
        debug_assert_eq!(input.geometry(), self.cfg.geometry);
        // Scheduled I pictures every `gop`; a P picture whose prediction is
        // worse than intra coding across the picture becomes an I picture and
        // restarts the GOP (the reference's default scene-change rule).
        let mut key = self.index == 0 || self.index - self.gop_start >= self.cfg.gop as u64;
        let same_source_luma = !key && input.y() == self.previous_luma;
        let mut f_code = 0;
        if !key {
            let (f, scene_change) = self.analyse(input, same_source_luma);
            f_code = f;
            key = scene_change;
        }
        let kind = if key { Kind::Intra } else { Kind::Predicted };
        if key {
            self.gop_start = self.index;
            self.intra_analysis(input);
            if self.rc.needs_intra_probe() {
                self.probe_intra(input);
            }
            self.write_sequence_header();
            self.write_gop_header();
        }
        self.write_picture_header(kind, f_code);
        let activity: f64 = self.row_activity.iter().sum();
        let header_bits = self.writer.bit_len();
        let plan = self.rc.begin(kind, activity, header_bits);
        let g = self.cfg.geometry;
        let total = (g.macroblock_width() * g.macroblock_height()) as f64;
        let mut q_sum = 0u64;
        let mut slice: Option<SliceState> = None;
        let mut economy = false;
        let mut censored = false;
        let mut activity_done = 0.0;
        for mb_y in 0..g.macroblock_height() {
            let spent = (self.writer.bit_len() - header_bits) as f64;
            let mut q = self.rc.row_q(spent, activity_done);
            let rows_left = (g.macroblock_height() - mb_y) as f64;
            // Conservative floor of what the remaining rows cost at minimum.
            let floor = rows_left
                * if key {
                    48.0 * g.macroblock_width() as f64 + 64.0
                } else {
                    96.0
                };
            let remaining_target = (plan.target - header_bits as f64).max(0.0);
            let planned_rest = remaining_target * (1.0 - activity_done / activity.max(1.0));
            // Escalate only when the remainder, at the planned pace or at its
            // minimum cost (whichever is larger), would no longer fit the VBV.
            if self.writer.bit_len() as f64 + floor.max(planned_rest) > plan.hard_limit * 0.97 {
                q = q.max(plan.q + 4).min(31);
            }
            economy |= self.writer.bit_len() as f64 + floor > plan.hard_limit - 8192.0;
            q_sum += q as u64 * g.macroblock_width() as u64;
            let mut s = slice.take().unwrap_or_else(|| {
                self.writer.start_code(1);
                self.writer.put(q, 5);
                self.writer.put(0, 1); // extra_bit_slice
                SliceState {
                    q,
                    target_q: q,
                    dc_pred: [128; 3],
                    pmv: Mv::ZERO,
                    last_intra: None,
                    skipped: 0,
                }
            });
            s.target_q = q;
            censored |=
                self.encode_row(input, mb_y, &mut s, kind, f_code, economy, plan.hard_limit);
            slice = Some(s);
            activity_done += self.row_activity[mb_y];
        }
        self.writer.take_into(out);
        // The emitted picture includes the writer's existing byte alignment.
        // Charge those real bits to both the reservoir and the average bank.
        let bits = out.len() * 8;
        if !same_source_luma {
            self.previous_luma.copy_from_slice(input.y());
        }
        let mean_q = q_sum as f64 / total;
        if censored {
            self.rc.end_censored(bits as f64, mean_q);
        } else {
            self.rc
                .end(bits as f64, (bits - header_bits) as f64, mean_q);
        }
        std::mem::swap(&mut self.reference, &mut self.recon);
        if !key {
            for (prev, a) in self.previous_mvs.iter_mut().zip(&self.analysis) {
                *prev = if a.intra { Mv::ZERO } else { a.mv };
            }
        } else {
            self.previous_mvs.fill(Mv::ZERO);
        }
        let picture = EncodedPicture {
            key,
            index: self.index,
            bits: out.len() * 8,
            qscale_mean: mean_q.round() as u32,
        };
        self.index += 1;
        picture
    }

    fn write_sequence_header(&mut self) {
        let g = self.cfg.geometry;
        let w = &mut self.writer;
        w.start_code(0xB3);
        w.put(g.width() as u32, 12);
        w.put(g.height() as u32, 12);
        w.put(1, 4); // pel_aspect_ratio: 1.0 (square)
        w.put(FRAME_RATE_CODE, 4);
        w.put(self.cfg.max_rate.div_ceil(400), 18);
        w.put(1, 1); // marker
        w.put(self.cfg.vbv_bits.div_ceil(16384).clamp(1, 1023), 10);
        w.put(0, 1); // constrained_parameters_flag
        w.put(0, 1); // load_intra_quantizer_matrix
        w.put(0, 1); // load_non_intra_quantizer_matrix
    }

    fn write_gop_header(&mut self) {
        let n = self.index;
        let w = &mut self.writer;
        w.start_code(0xB8);
        w.put(0, 1); // drop_frame_flag
        w.put(((n / (30 * 3600)) % 24) as u32, 5);
        w.put(((n / 1800) % 60) as u32, 6);
        w.put(1, 1); // marker
        w.put(((n / 30) % 60) as u32, 6);
        w.put((n % 30) as u32, 6);
        // closed_gop: set on the session's first GOP only, as the reference
        // does; with no B pictures the flag has no decoding consequence.
        w.put((n == 0) as u32, 1);
        w.put(0, 1); // broken_link
    }

    fn write_picture_header(&mut self, kind: Kind, f_code: u32) {
        let temporal = ((self.index - self.gop_start) % 1024) as u32;
        let w = &mut self.writer;
        w.start_code(0x00);
        w.put(temporal, 10);
        w.put(if kind == Kind::Intra { 1 } else { 2 }, 3);
        w.put(0xFFFF, 16); // vbv_delay: variable bit rate
        if kind == Kind::Predicted {
            w.put(0, 1); // full_pel_forward_vector
            w.put(f_code, 3);
        }
        w.put(0, 1); // extra_bit_picture
    }

    /// Motion analysis for a P picture; returns the forward f_code covering
    /// every chosen vector and whether the picture is a scene change.
    ///
    /// Scene score: Σ over macroblocks of √min(SSE, V + 100λ) − √(V + 20λ),
    /// with SSE the difference to the previous *source* picture at the
    /// full-sample part of the chosen vector, V the block's variance·256 and
    /// λ ≈ 0.85·q²; a positive total means the content changed more than
    /// intra coding costs. Comparing sources keeps the decision about the
    /// content rather than about the reconstruction's quantisation.
    fn analyse(&mut self, input: &Picture, same_source_luma: bool) -> (u32, bool) {
        let g = self.cfg.geometry;
        let stride = g.luma_stride();
        let lambda = self.rc.last_q.clamp(2, 31);
        let scene_lambda = (85 * lambda * lambda) as f64 / 100.0;
        let scene_floor = (20.0 * scene_lambda).sqrt();
        let mut scene = 0.0f64;
        self.unchanged.clear();
        let (mut lo, mut hi) = (0i32, 0i32);
        let rows = g.macroblock_height() * 16;
        motion::decimate4(input.y(), stride, rows, &mut self.coarse_cur);
        motion::decimate4(self.reference.y(), stride, rows, &mut self.coarse_ref);
        let coarse = Coarse {
            cur: &self.coarse_cur,
            reference: &self.coarse_ref,
            width: g.macroblock_width() * 4,
        };
        for mb_y in 0..g.macroblock_height() {
            let mut row = 0.0;
            for mb_x in 0..g.macroblock_width() {
                let i = mb_y * g.macroblock_width() + mb_x;
                let (x, y) = (mb_x * 16, mb_y * 16);
                let left = if mb_x > 0 {
                    self.analysis[i - 1].mv
                } else {
                    Mv::ZERO
                };
                let mut candidates = [left, self.previous_mvs[i], Mv::ZERO, Mv::ZERO, Mv::ZERO];
                if mb_y > 0 {
                    candidates[2] = self.analysis[i - g.macroblock_width()].mv;
                    if mb_x + 1 < g.macroblock_width() {
                        candidates[3] = self.analysis[i - g.macroblock_width() + 1].mv;
                    }
                }
                if mb_x + 1 < g.macroblock_width() {
                    candidates[4] = self.previous_mvs[i + 1];
                }
                let search = Search {
                    cur: input.y(),
                    reference: self.reference.y(),
                    stride,
                    x,
                    y,
                    bounds: Bounds::new(&g, mb_x, mb_y, SEARCH_RANGE),
                    pred: left,
                    lambda,
                    f_code: 2,
                };
                let neighbour = if mb_x > 0 {
                    self.analysis[i - 1].sad
                } else {
                    u32::MAX
                };
                let (mv, sad) = search.run(&candidates, &coarse, mb_x, mb_y, neighbour);
                // Intra only pays off when prediction is poor; skip the
                // deviation pass otherwise.
                let deviation = if sad > INTRA_MARGIN {
                    intra_activity(&input.y()[y * stride + x..], stride)
                } else {
                    u32::MAX
                };
                let intra = deviation.saturating_add(INTRA_MARGIN) < sad;
                // Rate-model activity: intra deviation or compensated SAD.
                row += if intra { deviation } else { sad } as f64;
                let origin = y * stride + x;
                let block = &input.y()[origin..];
                let full =
                    (y as i32 + (mv.y >> 1)) as usize * stride + (x as i32 + (mv.x >> 1)) as usize;
                // The scene metric compares two source blocks. A repeated
                // source proves equality only at the same integer offset;
                // motion-compensated blocks still need their own comparison.
                let sse = if sad == 0 || (same_source_luma && full == origin) {
                    0
                } else {
                    motion::sse16(block, stride, &self.previous_luma[full..], stride)
                };
                if sse == 0 {
                    // Unchanged content: its exact term −√(V + 20λ) is at most
                    // −√(20λ); the variance is only needed if that bound leaves
                    // the decision open (below).
                    scene -= scene_floor;
                    self.unchanged.push(i as u32);
                } else {
                    let variance = variance256(block, stride) as f64;
                    scene += (sse as f64).min(variance + 100.0 * scene_lambda).sqrt()
                        - (variance + 20.0 * scene_lambda).sqrt();
                }
                self.analysis[i] = Analysis { mv, sad, intra };
                if !intra {
                    lo = lo.min(mv.x).min(mv.y);
                    hi = hi.max(mv.x).max(mv.y);
                }
            }
            self.row_activity[mb_y] = row;
        }
        if scene > 0.0 {
            for &i in &self.unchanged {
                let (x, y) = (
                    (i as usize % g.macroblock_width()) * 16,
                    (i as usize / g.macroblock_width()) * 16,
                );
                let variance = variance256(&input.y()[y * stride + x..], stride) as f64;
                scene -= (variance + 20.0 * scene_lambda).sqrt() - scene_floor;
            }
        }
        let f_code = (1..=MAX_F_CODE)
            .find(|&f| lo >= -(16 << (f - 1)) && hi < 16 << (f - 1))
            .unwrap_or(MAX_F_CODE);
        (f_code, scene > 0.0)
    }

    /// Per-row intra activity of an I picture.
    fn intra_analysis(&mut self, input: &Picture) {
        let g = self.cfg.geometry;
        let stride = g.luma_stride();
        for mb_y in 0..g.macroblock_height() {
            let mut row = 0.0;
            for mb_x in 0..g.macroblock_width() {
                let cost = intra_activity(&input.y()[mb_y * 16 * stride + mb_x * 16..], stride);
                self.analysis[mb_y * g.macroblock_width() + mb_x] = Analysis {
                    mv: Mv::ZERO,
                    sad: 0,
                    intra: true,
                };
                row += cost as f64;
            }
            self.row_activity[mb_y] = row;
        }
    }

    /// First intra picture of a session: trial-encode every second row at a
    /// fixed quantiser into scratch bits to seed the intra rate model. The
    /// reconstruction it leaves behind is overwritten by the real pass.
    fn probe_intra(&mut self, input: &Picture) {
        const PROBE_Q: u32 = 8;
        let g = self.cfg.geometry;
        let mut scratch = BitWriter::with_capacity(4096);
        std::mem::swap(&mut self.writer, &mut scratch);
        let mut activity = 0.0;
        for mb_y in (0..g.macroblock_height()).step_by(2) {
            let mut s = SliceState {
                q: PROBE_Q,
                target_q: PROBE_Q,
                dc_pred: [128; 3],
                pmv: Mv::ZERO,
                last_intra: None,
                skipped: 0,
            };
            for mb_x in 0..g.macroblock_width() {
                self.intra_mb(
                    input,
                    mb_x,
                    mb_y,
                    mb_y * g.macroblock_width() + mb_x,
                    &mut s,
                    Kind::Intra,
                    false,
                );
            }
            activity += self.row_activity[mb_y];
        }
        let bits = self.writer.bit_len() as f64;
        std::mem::swap(&mut self.writer, &mut scratch);
        self.rc.seed_intra(bits, PROBE_Q, activity);
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_row(
        &mut self,
        input: &Picture,
        mb_y: usize,
        s: &mut SliceState,
        kind: Kind,
        f_code: u32,
        economy: bool,
        hard_limit: f64,
    ) -> bool {
        let g = self.cfg.geometry;
        let last = g.macroblock_width() * g.macroblock_height() - 1;
        let mut censored = false;
        for mb_x in 0..g.macroblock_width() {
            let address = mb_y * g.macroblock_width() + mb_x;
            // The first and last macroblock of a slice are never skipped.
            let edge = address == 0 || address == last;
            // Hard VBV guard: past this point only the cheapest legal codes.
            let tight = economy || self.writer.bit_len() as f64 > hard_limit - 8192.0;
            censored |= tight;
            match kind {
                Kind::Intra => self.intra_mb(input, mb_x, mb_y, address, s, Kind::Intra, tight),
                Kind::Predicted => {
                    let a = self.analysis[address];
                    if a.intra && !tight {
                        self.intra_mb(input, mb_x, mb_y, address, s, Kind::Predicted, false);
                    } else {
                        let mv = if a.intra { Mv::ZERO } else { a.mv };
                        self.inter_mb(input, mb_x, mb_y, address, s, mv, f_code, edge, tight);
                    }
                }
            }
        }
        censored
    }

    fn write_address(&mut self, s: &mut SliceState) {
        let mut increment = s.skipped + 1;
        while increment > 33 {
            self.writer.put(ADDRESS_ESCAPE.0, ADDRESS_ESCAPE.1);
            increment -= 33;
        }
        let (code, len) = ADDRESS_INCREMENT[increment];
        self.writer.put(code, len);
        s.skipped = 0;
    }

    #[allow(clippy::too_many_arguments)]
    fn intra_mb(
        &mut self,
        input: &Picture,
        mb_x: usize,
        mb_y: usize,
        address: usize,
        s: &mut SliceState,
        kind: Kind,
        dc_only: bool,
    ) {
        let g = self.cfg.geometry;
        if s.last_intra.is_none_or(|a| address - a > 1) {
            s.dc_pred = [128; 3];
        }
        self.write_address(s);
        let requant = s.target_q != s.q;
        let (code, len) = match (kind, requant) {
            (Kind::Intra, false) => MB_I_INTRA,
            (Kind::Intra, true) => MB_I_INTRA_QUANT,
            (Kind::Predicted, false) => MB_P_INTRA,
            (Kind::Predicted, true) => MB_P_INTRA_QUANT,
        };
        self.writer.put(code, len);
        if requant {
            self.writer.put(s.target_q, 5);
            s.q = s.target_q;
        }
        let mut coeffs = [0i32; 64];
        let mut levels = [0i32; 64];
        for b in 0..6 {
            let (plane, stride, x, y, comp) = block_origin(g, b, mb_x, mb_y);
            let src = match plane {
                0 => &input.y(),
                1 => &input.cb(),
                _ => &input.cr(),
            };
            for r in 0..8 {
                let row = &src[(y + r) * stride + x..(y + r) * stride + x + 8];
                for c in 0..8 {
                    coeffs[r * 8 + c] = row[c] as i32;
                }
            }
            dct::fdct(&mut coeffs);
            let dc = ((coeffs[0] + 4) >> 3).clamp(0, 255);
            block::write_dc(&mut self.writer, dc - s.dc_pred[comp], comp == 0);
            s.dc_pred[comp] = dc;
            let mask = if dc_only {
                0
            } else {
                self.quant.intra(&coeffs, s.q, &mut levels)
            };
            block::write_coefficients(&mut self.writer, &levels, mask, true);
            block::dequant_intra(&levels, mask, dc, s.q, &mut coeffs);
            dct::idct(&mut coeffs);
            let dst = match plane {
                0 => self.recon.y_mut(),
                1 => self.recon.cb_mut(),
                _ => self.recon.cr_mut(),
            };
            for r in 0..8 {
                let row = &mut dst[(y + r) * stride + x..(y + r) * stride + x + 8];
                for c in 0..8 {
                    row[c] = coeffs[r * 8 + c].clamp(0, 255) as u8;
                }
            }
        }
        s.last_intra = Some(address);
        s.pmv = Mv::ZERO;
    }

    #[allow(clippy::too_many_arguments)]
    fn inter_mb(
        &mut self,
        input: &Picture,
        mb_x: usize,
        mb_y: usize,
        address: usize,
        s: &mut SliceState,
        mv: Mv,
        f_code: u32,
        edge: bool,
        tight: bool,
    ) {
        let g = self.cfg.geometry;
        let mut pred = [[0u8; 64]; 6];
        let ls = g.luma_stride();
        motion::predict_luma_blocks(
            self.reference.y(),
            ls,
            mb_x * 16,
            mb_y * 16,
            mv.x,
            mv.y,
            &mut pred,
        );
        let cs = g.chroma_stride();
        let (cx, cy) = (motion::chroma(mv.x), motion::chroma(mv.y));
        motion::predict::<8>(
            self.reference.cb(),
            cs,
            mb_x * 8,
            mb_y * 8,
            cx,
            cy,
            &mut pred[4],
        );
        motion::predict::<8>(
            self.reference.cr(),
            cs,
            mb_x * 8,
            mb_y * 8,
            cx,
            cy,
            &mut pred[5],
        );

        let mut masks = [0u64; 6];
        let mut cbp = 0u32;
        if !tight {
            let mut coeffs = [0i32; 64];
            for b in 0..6 {
                let (plane, stride, x, y, _) = block_origin(g, b, mb_x, mb_y);
                let src = match plane {
                    0 => &input.y(),
                    1 => &input.cb(),
                    _ => &input.cr(),
                };
                let mut magnitude = 0u32;
                for r in 0..8 {
                    let row: &[u8; 8] = src[(y + r) * stride + x..(y + r) * stride + x + 8]
                        .try_into()
                        .unwrap();
                    let p: &[u8; 8] = pred[b][r * 8..r * 8 + 8].try_into().unwrap();
                    for c in 0..8 {
                        let d = row[c] as i32 - p[c] as i32;
                        coeffs[r * 8 + c] = d;
                        magnitude += d.unsigned_abs();
                    }
                }
                if self.quant.inter_is_zero(magnitude, s.target_q) {
                    continue;
                }
                dct::fdct(&mut coeffs);
                masks[b] = self.quant.inter(&coeffs, s.target_q, &mut self.levels[b]);
                if masks[b] != 0 {
                    cbp |= 32 >> b;
                }
            }
        }

        let zero = mv == Mv::ZERO;
        if cbp == 0 && zero && !edge {
            // Skipped: zero vector, prediction copied, predictors reset.
            s.skipped += 1;
            s.pmv = Mv::ZERO;
            self.store_prediction(mb_x, mb_y, &pred);
            return;
        }
        self.write_address(s);
        let requant = cbp != 0 && s.target_q != s.q;
        match (cbp != 0, zero) {
            (true, true) => {
                let (code, len) = if requant {
                    MB_P_NOMC_CODED_QUANT
                } else {
                    MB_P_NOMC_CODED
                };
                self.writer.put(code, len);
                if requant {
                    self.writer.put(s.target_q, 5);
                    s.q = s.target_q;
                }
                s.pmv = Mv::ZERO;
            }
            (true, false) => {
                let (code, len) = if requant {
                    MB_P_MC_CODED_QUANT
                } else {
                    MB_P_MC_CODED
                };
                self.writer.put(code, len);
                if requant {
                    self.writer.put(s.target_q, 5);
                    s.q = s.target_q;
                }
                self.write_mv(s, mv, f_code);
            }
            (false, _) => {
                self.writer.put(MB_P_MC_NOT_CODED.0, MB_P_MC_NOT_CODED.1);
                self.write_mv(s, mv, f_code);
            }
        }
        if cbp != 0 {
            let (code, len) = CODED_BLOCK_PATTERN[cbp as usize];
            self.writer.put(code, len);
        }
        let mut coeffs = [0i32; 64];
        for b in 0..6 {
            if cbp & (32 >> b) == 0 {
                continue;
            }
            block::write_coefficients(&mut self.writer, &self.levels[b], masks[b], false);
            block::dequant_inter(&self.levels[b], masks[b], s.q, &mut coeffs);
            dct::idct(&mut coeffs);
            for (p, &c) in pred[b].iter_mut().zip(&coeffs) {
                *p = (*p as i32 + c).clamp(0, 255) as u8;
            }
        }
        self.store_prediction(mb_x, mb_y, &pred);
        let _ = address;
    }

    fn write_mv(&mut self, s: &mut SliceState, mv: Mv, f_code: u32) {
        for (v, p) in [(mv.x, s.pmv.x), (mv.y, s.pmv.y)] {
            write_mv_component(&mut self.writer, v - p, f_code);
        }
        s.pmv = mv;
    }

    fn store_prediction(&mut self, mb_x: usize, mb_y: usize, blocks: &[[u8; 64]; 6]) {
        let g = self.cfg.geometry;
        for (b, block) in blocks.iter().enumerate() {
            let (plane, stride, x, y, _) = block_origin(g, b, mb_x, mb_y);
            let dst = match plane {
                0 => self.recon.y_mut(),
                1 => self.recon.cb_mut(),
                _ => self.recon.cr_mut(),
            };
            for r in 0..8 {
                dst[(y + r) * stride + x..(y + r) * stride + x + 8]
                    .copy_from_slice(&block[r * 8..r * 8 + 8]);
            }
        }
    }
}

/// Plane (0 Y, 1 Cb, 2 Cr), stride, sample origin and DC-predictor component of block `b`.
#[inline]
fn block_origin(
    g: Geometry,
    b: usize,
    mb_x: usize,
    mb_y: usize,
) -> (usize, usize, usize, usize, usize) {
    match b {
        0..=3 => (
            0,
            g.luma_stride(),
            mb_x * 16 + (b & 1) * 8,
            mb_y * 16 + (b >> 1) * 8,
            0,
        ),
        4 => (1, g.chroma_stride(), mb_x * 8, mb_y * 8, 1),
        _ => (2, g.chroma_stride(), mb_x * 8, mb_y * 8, 2),
    }
}

/// Σx² − (Σx)²/256 over a 16×16 block: 256 × its variance.
fn variance256(block: &[u8], stride: usize) -> u32 {
    let (mut sum, mut squares) = ([0u32; 16], [0u32; 16]);
    for r in 0..16 {
        let row: &[u8; 16] = block[r * stride..r * stride + 16].try_into().unwrap();
        for i in 0..16 {
            sum[i] += row[i] as u32;
            squares[i] += row[i] as u32 * row[i] as u32;
        }
    }
    let (sum, squares): (u32, u32) = (sum.iter().sum(), squares.iter().sum());
    squares - (sum * sum) / 256
}

/// Mean absolute deviation of a 16×16 block from its mean (intra cost proxy).
fn intra_activity(block: &[u8], stride: usize) -> u32 {
    let mut sum = 0u32;
    for r in 0..16 {
        sum += block[r * stride..r * stride + 16]
            .iter()
            .map(|&v| v as u32)
            .sum::<u32>();
    }
    let mean = (sum + 128) / 256;
    let mut dev = 0u32;
    for r in 0..16 {
        dev += block[r * stride..r * stride + 16]
            .iter()
            .map(|&v| (v as u32).abs_diff(mean))
            .sum::<u32>();
    }
    dev
}

/// motion_code, sign and motion_r for one differential component, wrapped
/// into the f_code range as permitted by the modular reconstruction rule.
fn write_mv_component(w: &mut BitWriter, delta: i32, f_code: u32) {
    let r_size = f_code - 1;
    let f = 1i32 << r_size;
    let (lo, hi) = (-16 * f, 16 * f - 1);
    let mut d = delta;
    if d < lo {
        d += 32 * f;
    } else if d > hi {
        d -= 32 * f;
    }
    if d == 0 {
        w.put(MOTION_CODE[0].0, MOTION_CODE[0].1);
        return;
    }
    let magnitude = d.unsigned_abs() - 1;
    let code = (magnitude >> r_size) + 1;
    let (c, l) = MOTION_CODE[code as usize];
    w.put(c << 1 | (d < 0) as u32, l + 1);
    if r_size > 0 {
        w.put(magnitude & (f as u32 - 1), r_size);
    }
}

#[cfg(test)]
mod zero_residual_controls;
