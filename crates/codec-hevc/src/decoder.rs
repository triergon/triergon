//! Picture-level decoding: NAL dispatch, parameter set activation, POC (8.3.1), RPS (8.3.2),
//! unavailable reference generation (8.3.3), reference picture lists (8.3.4), collocated
//! picture (8.3.5) and in-loop filters (8.7). The decoded picture buffer, its marking and
//! its output and removal (C.5.2) are `triergon-framing`'s `DecodedPictures`.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::ctu::{RefEntry, RefLists, Scratch, SliceDecoder};
use crate::error::{DecodeError, Result};
use crate::nal::{self, NalHeader};
use crate::output::{ColourDescription, OutInfo, Picture};
use crate::picture::{AnyFrame, ColMotion, ColMv, Frame, FramePool, Pixel, SharedFrame};
use crate::ps::{self, Layout, Pps, Shared, Sps};
use crate::slice::{self, SLICE_B, SLICE_I, SliceHeader};
use crate::state::{PicState, SliceInfo};
use crate::{deblock, sao};
use triergon_codec::{Budget, BudgetVec, Charged, PictureView, VideoDecoder};
use triergon_framing::picture_output::{
    DecodedPictures, HevcLimits, HevcPicture, Leave, Marking, NewPicture, Parity, View,
};
use triergon_framing::{hevc, nal::AnnexBUnits};

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Bytes of live storage that depends on the stream: decoded and reference pictures
    /// (also while a caller holds them), the frame pool, collocated motion, per-picture
    /// block and CTB maps, parameter sets, tile and scan tables, the SAO band buffer and
    /// the RBSP of the NAL unit being parsed.
    ///
    /// The default, 1.25 GiB, holds the largest picture the level 6.2 size cap accepts
    /// (8192×4352, 107 MB in 10-bit storage) in the 6-picture DPB level 6.2 allows at
    /// that size, plus the frame pool and two held output pictures (nine frames,
    /// 963 MB), and about 70 MB of motion, maps and tables, with about 30% to spare.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 1280 << 20,
        }
    }
}

/// The session view of the decoder. A received picture is held until the next call, so
/// its frame returns to the pool exactly when the inherent API's would.
impl VideoDecoder for Decoder {
    type Error = DecodeError;

    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<()> {
        self.ctx.held = None;
        Decoder::send(self, au, pts)
    }
    fn receive(&mut self) -> Option<PictureView<'_>> {
        self.ctx.held = self.ctx.out.pop_front();
        Some(self.ctx.held.as_ref()?.view())
    }
    fn flush(&mut self) {
        self.ctx.held = None;
        Decoder::flush(self);
    }
    fn reset(&mut self) {
        self.ctx.held = None;
        Decoder::reset(self);
    }
}

/// A picture of the decoded picture buffer, which keeps its order count and marking.
#[derive(Debug)]
struct DpbPic {
    frame: Arc<SharedFrame>,
    col: Arc<ColMotion>,
    pts: Option<i64>,
    info: Arc<OutInfo>,
}

#[derive(Debug)]
struct CurPic {
    frame: AnyFrame,
    poc: i32,
    sps: Shared<Sps>,
    pps: Shared<Pps>,
    layout: Arc<Layout>,
    output: bool,
    pts: Option<i64>,
    prev_sh: Option<SliceHeader>,
    /// DPB indices for RefPicSetStCurrBefore / StCurrAfter / LtCurr.
    st_before: Vec<usize>,
    st_after: Vec<usize>,
    lt_curr: Vec<usize>,
    info: Arc<OutInfo>,
    /// Collocated motion of this picture, filled when it is finished.
    col: ColMotion,
}

/// H.265 decoder of Annex B access units or NAL units into [`Picture`]s in output order.
///
/// A failed call abandons the picture it was decoding (what was decoded is still output)
/// and leaves the decoder usable for the next access unit.
#[derive(Debug)]
pub struct Decoder {
    /// The RBSP of the NAL unit being decoded, kept beside the context that reads it.
    rbsp: BudgetVec<u8>,
    ctx: Context,
}

/// Everything but the RBSP buffer: parameter sets, DPB, current picture and its state.
#[derive(Debug)]
struct Context {
    sps: Vec<Option<Shared<Sps>>>,
    pps: Vec<Option<Shared<Pps>>>,
    layout_cache: Option<(Shared<Sps>, Shared<Pps>, Arc<Layout>)>,
    dpb: DecodedPictures<DpbPic>,
    out: VecDeque<Picture>,
    /// The picture the last [`VideoDecoder::receive`] view borrows.
    held: Option<Picture>,
    cur: Option<CurPic>,
    st: PicState,
    scratch8: Scratch<u8>,
    scratch16: Scratch<u16>,
    pool: Arc<FramePool>,
    /// The pool every stream-sized buffer of this decoder is charged to.
    budget: Budget,
    // POC / IRAP state
    prev_tid0_poc: i32,
    first_picture: bool,
    after_eos: bool,
    no_rasl_output: bool,
    seen_irap: bool,
    /// The timestamp the next picture started takes.
    pending_pts: Option<i64>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with its own pool of the default [`Limits`].
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Budget`] when the limit is below the pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging every stream-sized buffer to the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Decoder {
            rbsp: BudgetVec::new(budget),
            ctx: Context::new(budget),
        }
    }

    /// Decode one Annex B access unit; the picture it carries takes the opaque `pts`.
    /// The picture is complete when the call returns; it is output once the decoded
    /// picture buffer's reorder rules release it (see [`Decoder::receive`]).
    ///
    /// # Errors
    ///
    /// The first error of a NAL unit of the access unit; the later units are not decoded.
    pub fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<()> {
        self.ctx.pending_pts = pts;
        let mut result = Ok(());
        for nalu in AnnexBUnits::new(au) {
            if let Err(e) = self.decode_nal(nalu) {
                // Abandon the current picture on error but keep the decoder usable.
                if self.ctx.cur.is_some() {
                    let _ = self.ctx.finish_picture();
                }
                result = Err(e);
                break;
            }
        }
        // One access unit per call: the picture is complete.
        if self.ctx.cur.is_some() {
            self.ctx.finish_picture()?;
        }
        result
    }

    /// Decode one NAL unit without start code. A picture is complete when the next one
    /// starts, at an end of sequence or bitstream unit, or on [`Decoder::flush`]. A `pts`
    /// given here replaces the one the next picture started takes.
    ///
    /// # Errors
    ///
    /// When the unit is invalid, unsupported, refers to a missing parameter set, or its
    /// storage does not fit the budget.
    pub fn send_nal(&mut self, nalu: &[u8], pts: Option<i64>) -> Result<()> {
        if pts.is_some() {
            self.ctx.pending_pts = pts;
        }
        self.decode_nal(nalu)
    }

    /// Pop the next picture in output order, if one is ready.
    pub fn receive(&mut self) -> Option<Picture> {
        self.ctx.out.pop_front()
    }

    /// End of stream: complete the current picture and output every remaining one.
    pub fn flush(&mut self) {
        self.ctx.flush();
    }

    /// Discontinuity: drop the decoded picture buffer, the pictures not yet received and
    /// any partially decoded picture. Parameter sets are kept.
    pub fn reset(&mut self) {
        self.ctx.reset();
    }

    fn decode_nal(&mut self, nalu: &[u8]) -> Result<()> {
        let Some(h) = NalHeader::parse(nalu) else {
            return Ok(());
        };
        if h.nuh_layer_id > 0 {
            return Ok(());
        }
        match h.nal_unit_type {
            hevc::EOS | hevc::EOB => {
                if self.ctx.cur.is_some() {
                    self.ctx.finish_picture()?;
                }
                self.ctx.after_eos = true;
            }
            0..=9 | 16..=21 | hevc::VPS | hevc::SPS | hevc::PPS => {
                nal::unescape_rbsp(nalu, &mut self.rbsp)?;
                self.ctx.decode_rbsp(&self.rbsp, h)?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl Context {
    fn new(budget: &Budget) -> Self {
        Context {
            sps: (0..ps::MAX_SPS).map(|_| None).collect(),
            pps: (0..ps::MAX_PPS).map(|_| None).collect(),
            layout_cache: None,
            dpb: DecodedPictures::new(budget),
            out: VecDeque::new(),
            held: None,
            cur: None,
            st: PicState::new(budget),
            scratch8: Scratch::new(budget),
            scratch16: Scratch::new(budget),
            pool: Arc::default(),
            budget: budget.clone(),
            prev_tid0_poc: 0,
            first_picture: true,
            after_eos: false,
            no_rasl_output: true,
            seen_irap: false,
            pending_pts: None,
        }
    }

    fn flush(&mut self) {
        if self.cur.is_some() {
            let _ = self.finish_picture();
        }
        self.dpb.flush(output(&mut self.out));
    }

    fn reset(&mut self) {
        self.cur = None;
        self.dpb.discard(|_| {});
        self.out.clear();
        self.first_picture = true;
        self.seen_irap = false;
        self.no_rasl_output = true;
    }

    fn decode_rbsp(&mut self, rbsp: &[u8], h: NalHeader) -> Result<()> {
        match h.nal_unit_type {
            // Nothing of the VPS is used; it is still refused when cut short.
            hevc::VPS => ps::parse_vps(rbsp)?,
            hevc::SPS => {
                let s = ps::parse_sps(rbsp)?;
                let id = s.id as usize;
                self.sps[id] = Some(Arc::new(Charged::boxed(&self.budget, s)?));
            }
            hevc::PPS => {
                let p = ps::parse_pps(rbsp)?;
                let id = p.id as usize;
                self.pps[id] = Some(Arc::new(Charged::boxed(&self.budget, p)?));
            }
            _ => self.decode_slice_nal(rbsp, h)?,
        }
        Ok(())
    }

    fn decode_slice_nal(&mut self, rbsp: &[u8], h: NalHeader) -> Result<()> {
        let t = h.nal_unit_type;
        let (first, pps_id) = slice::peek_pps_id(rbsp, t)?;
        if first && self.cur.is_some() {
            self.finish_picture()?;
        }
        if first {
            // RASL pictures associated with an IRAP with NoRaslOutputFlag = 1 are not decoded.
            if hevc::is_irap(t) {
                self.seen_irap = true;
                self.no_rasl_output =
                    hevc::is_idr(t) || hevc::is_bla(t) || self.first_picture || self.after_eos;
            } else if !self.seen_irap {
                return Ok(());
            }
            if hevc::is_rasl(t) && self.no_rasl_output {
                return Ok(());
            }
        } else if self.cur.is_none() {
            // Continuation of a skipped (or lost) picture.
            return Ok(());
        }
        let pps = self.pps[pps_id as usize]
            .clone()
            .ok_or(DecodeError::MissingParameterSet("PPS"))?;
        let sps = self.sps[pps.sps_id as usize]
            .clone()
            .ok_or(DecodeError::MissingParameterSet("SPS"))?;
        if !first
            && let Some(cur) = &self.cur
            && !Arc::ptr_eq(&cur.pps, &pps)
            && cur.pps.id != pps.id
        {
            return Err(DecodeError::InvalidData("PPS changed within a picture"));
        }
        let prev = self.cur.as_ref().and_then(|c| c.prev_sh.as_ref());
        let sh = slice::parse_slice_header(
            rbsp,
            t,
            h.temporal_id,
            &sps,
            &pps,
            if first { None } else { prev },
        )?;
        if first {
            self.start_picture(&sh, h, sps.clone(), pps.clone())?;
        }
        self.st.reserve_slice()?;
        let Some(cur) = self.cur.as_mut() else {
            return Ok(());
        };
        // Reference picture lists (8.3.4) and SliceInfo.
        let mut info = SliceInfo {
            beta_offset_div2: sh.beta_offset_div2,
            tc_offset_div2: sh.tc_offset_div2,
            lf_across_slices: sh.loop_filter_across_slices,
            ref_poc: [[i32::MIN; 16]; 2],
            ref_lt: [[false; 16]; 2],
        };
        let mut lists: [[Option<usize>; 16]; 2] = [[None; 16]; 2];
        let dpb: Vec<View<'_, DpbPic>> = self.dpb.entries().collect();
        if sh.slice_type != SLICE_I {
            let mut temp: Vec<(usize, bool)> = Vec::with_capacity(32);
            let total = cur.st_before.len() + cur.st_after.len() + cur.lt_curr.len();
            if total == 0 {
                return Err(DecodeError::InvalidData("no reference pictures"));
            }
            for l in 0..(if sh.slice_type == SLICE_B { 2 } else { 1 }) {
                temp.clear();
                let n = (sh.num_ref_idx[l] as usize).max(total);
                while temp.len() < n {
                    let (a, b) = if l == 0 {
                        (&cur.st_before, &cur.st_after)
                    } else {
                        (&cur.st_after, &cur.st_before)
                    };
                    for &i in a {
                        if temp.len() < n {
                            temp.push((i, false));
                        }
                    }
                    for &i in b {
                        if temp.len() < n {
                            temp.push((i, false));
                        }
                    }
                    for &i in &cur.lt_curr {
                        if temp.len() < n {
                            temp.push((i, true));
                        }
                    }
                }
                for r in 0..sh.num_ref_idx[l] as usize {
                    let e = if sh.rpl_mod[l] {
                        temp[sh.list_entry[l][r] as usize]
                    } else {
                        temp[r]
                    };
                    lists[l][r] = Some(e.0);
                    info.ref_poc[l][r] = dpb[e.0].poc();
                    info.ref_lt[l][r] = e.1;
                }
            }
        }
        // A slice counts once its state is recorded, so a refused slice leaves nothing,
        // neither an index nor a header, that later slices of the picture could reach.
        let slice_idx = u16::try_from(self.st.slices.len())
            .map_err(|_| DecodeError::InvalidData("too many slice segments in a picture"))?;
        // Cannot fail: `reserve_slice` made room before the picture state changed.
        self.st.slices.try_push(info).map_err(|e| e.error)?;
        if !sh.dependent_slice_segment {
            cur.prev_sh = Some(sh.clone());
        }
        let layout = cur.layout.clone();
        let poc = cur.poc;

        match &mut cur.frame {
            AnyFrame::U8(f) => decode_slice_generic::<u8>(
                &sps,
                &pps,
                &sh,
                &layout,
                f,
                &mut self.st,
                &dpb,
                &lists,
                &mut self.scratch8,
                rbsp,
                slice_idx,
                poc,
            ),
            AnyFrame::U16(f) => decode_slice_generic::<u16>(
                &sps,
                &pps,
                &sh,
                &layout,
                f,
                &mut self.st,
                &dpb,
                &lists,
                &mut self.scratch16,
                rbsp,
                slice_idx,
                poc,
            ),
        }
    }

    fn start_picture(
        &mut self,
        sh: &SliceHeader,
        h: NalHeader,
        sps: Shared<Sps>,
        pps: Shared<Pps>,
    ) -> Result<()> {
        let t = h.nal_unit_type;
        let irap = hevc::is_irap(t);
        if let Some(tool) = pps.range_ext_tool
            && sps.profile_idc >= 4
        {
            return Err(DecodeError::Unsupported(tool));
        }
        if pps.scc_ext && sps.profile_idc >= 9 {
            return Err(DecodeError::Unsupported("pps_scc_extension (SCC)"));
        }
        let layout = match &self.layout_cache {
            Some((s, p, l)) if Arc::ptr_eq(s, &sps) && Arc::ptr_eq(p, &pps) => l.clone(),
            _ => {
                // Release the previous layout before the new one is charged.
                self.layout_cache = None;
                let l = Arc::new(Layout::new(&sps, &pps, &self.budget)?);
                self.layout_cache = Some((sps.clone(), pps.clone(), l.clone()));
                l
            }
        };
        // POC (8.3.1)
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = sh.poc_lsb as i32;
        let poc_msb = if irap && self.no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = poc_msb + lsb;
        // RPS (8.3.2)
        if irap && self.no_rasl_output {
            self.dpb.mark_unused();
        }
        let mut st_before_poc = Vec::new();
        let mut st_after_poc = Vec::new();
        let mut st_foll_poc = Vec::new();
        let mut lt_curr_poc: Vec<(i32, bool)> = Vec::new();
        let mut lt_foll_poc: Vec<(i32, bool)> = Vec::new();
        if !hevc::is_idr(t) {
            let rps = &sh.st_rps;
            for i in 0..rps.num_negative as usize {
                let (d, u) = rps.s0(i);
                if u {
                    st_before_poc.push(poc + d);
                } else {
                    st_foll_poc.push(poc + d);
                }
            }
            for i in 0..rps.num_positive as usize {
                let (d, u) = rps.s1(i);
                if u {
                    st_after_poc.push(poc + d);
                } else {
                    st_foll_poc.push(poc + d);
                }
            }
            for lt in sh.long_term() {
                let mut p = lt.poc_lsb as i32;
                if lt.msb_present {
                    p += poc
                        - (lt.delta_poc_msb_cycle as i32).wrapping_mul(max_lsb)
                        - (poc & (max_lsb - 1));
                }
                if lt.used_by_curr {
                    lt_curr_poc.push((p, lt.msb_present));
                } else {
                    lt_foll_poc.push((p, lt.msb_present));
                }
            }
        }
        let mut keep = vec![false; self.dpb.len()];
        // A long-term entry names a reference picture by its order count or its LSBs.
        let lt_poc = |d: &View<'_, DpbPic>, p: i32, msb: bool| {
            if msb {
                d.poc() == p
            } else {
                (d.poc() & (max_lsb - 1)) == p
            }
        };
        let find_lt = |dpb: &DecodedPictures<DpbPic>, p: i32, msb: bool| {
            position(dpb, |d| d.is_reference() && lt_poc(d, p, msb))
        };
        let lt_curr_idx: Vec<Option<usize>> = lt_curr_poc
            .iter()
            .map(|&(p, m)| find_lt(&self.dpb, p, m))
            .collect();
        let lt_foll_idx: Vec<Option<usize>> = lt_foll_poc
            .iter()
            .map(|&(p, m)| find_lt(&self.dpb, p, m))
            .collect();
        for &i in lt_curr_idx.iter().chain(lt_foll_idx.iter()).flatten() {
            keep[i] = true;
        }
        for (keep, mut d) in keep.iter().zip(self.dpb.entries_mut()) {
            if *keep {
                d.set_marking(None, Marking::LongTerm);
            }
        }
        let find_st = |dpb: &DecodedPictures<DpbPic>, p: i32| {
            position(dpb, |d| {
                d.marking(Parity::Top) == Marking::ShortTerm && d.poc() == p
            })
        };
        let st_before_idx: Vec<Option<usize>> = st_before_poc
            .iter()
            .map(|&p| find_st(&self.dpb, p))
            .collect();
        let st_after_idx: Vec<Option<usize>> = st_after_poc
            .iter()
            .map(|&p| find_st(&self.dpb, p))
            .collect();
        let st_foll_idx: Vec<Option<usize>> =
            st_foll_poc.iter().map(|&p| find_st(&self.dpb, p)).collect();
        for &i in st_before_idx
            .iter()
            .chain(st_after_idx.iter())
            .chain(st_foll_idx.iter())
            .flatten()
        {
            keep[i] = true;
        }
        for (keep, mut d) in keep.iter().zip(self.dpb.entries_mut()) {
            if !*keep {
                d.set_marking(None, Marking::Unused);
            }
        }
        // Picture output info.
        let vui = &sps.vui;
        let info = Arc::new(OutInfo {
            crop: sps.conf_win,
            width: sps.width,
            height: sps.height,
            bit_depth: sps.bit_depth_luma,
            sar: vui.sar,
            colour: ColourDescription {
                primaries: vui.colour_primaries,
                transfer: vui.transfer_characteristics,
                matrix: vui.matrix_coeffs,
                full_range: vui.full_range,
                video_signal_type_present: vui.video_signal_type_present,
                colour_description_present: vui.colour_description_present,
            },
        });
        // C.5.2.2 output and removal of pictures before decoding the current picture.
        self.dpb.hevc_remove(
            HevcPicture {
                irap_no_rasl_output: irap && self.no_rasl_output,
                cra: t == hevc::CRA,
                first: self.first_picture,
                no_output_of_prior_pics: sh.no_output_of_prior_pics,
            },
            limits(&sps),
            output(&mut self.out),
        );
        // The current picture's storage is charged once the removal above has freed what it
        // can (into the frame pool), and before the missing references and the POC state:
        // the buffer's room for the references generated below and the picture first.
        self.dpb
            .try_reserve(st_before_poc.len() + st_after_poc.len() + lt_curr_poc.len() + 1)?;
        let (w, hgt) = (sps.width as usize, sps.height as usize);
        let frame = if sps.high_bit_depth() {
            AnyFrame::U16(match self.pool.take(w, hgt) {
                Some(f) => f,
                None => Frame::try_new(&self.budget, w, hgt, 0)?,
            })
        } else {
            AnyFrame::U8(match self.pool.take(w, hgt) {
                Some(f) => f,
                None => Frame::try_new(&self.budget, w, hgt, 0)?,
            })
        };
        let ctb = sps.ctb_size as usize;
        match &frame {
            AnyFrame::U8(_) => prepare_band(&mut self.scratch8, w, ctb, &self.budget)?,
            AnyFrame::U16(_) => prepare_band(&mut self.scratch16, w, ctb, &self.budget)?,
        }
        let col = ColMotion::try_new(&self.budget, w, hgt)?;
        self.st
            .reset(w, hgt, sps.pic_size_ctb() as usize, sps.pic_h_ctb as usize)?;
        // The indices computed above are invalid if the DPB changed; recompute by POC.
        let resolve = |dpb: &DecodedPictures<DpbPic>, p: i32| {
            position(dpb, |d| d.is_reference() && d.poc() == p)
        };
        let mut st_before = Vec::new();
        let mut st_after = Vec::new();
        let mut lt_curr = Vec::new();
        // Generate missing references (8.3.3-style, also used for lost pictures).
        for (k, &p) in st_before_poc.iter().enumerate() {
            let i = match resolve(&self.dpb, p) {
                Some(i) if st_before_idx[k].is_some() => i,
                _ => self.generate_missing(&sps, p, Marking::ShortTerm, info.clone())?,
            };
            st_before.push(i);
        }
        for (k, &p) in st_after_poc.iter().enumerate() {
            let i = match resolve(&self.dpb, p) {
                Some(i) if st_after_idx[k].is_some() => i,
                _ => self.generate_missing(&sps, p, Marking::ShortTerm, info.clone())?,
            };
            st_after.push(i);
        }
        for (k, &(p, msb)) in lt_curr_poc.iter().enumerate() {
            let found = if lt_curr_idx[k].is_some() {
                position(&self.dpb, |d| {
                    d.marking(Parity::Top) == Marking::LongTerm && lt_poc(d, p, msb)
                })
            } else {
                None
            };
            let i = match found {
                Some(i) => i,
                None => self.generate_missing(&sps, p, Marking::LongTerm, info.clone())?,
            };
            lt_curr.push(i);
        }
        // Commit the POC and IRAP state.
        if h.temporal_id == 0
            && !hevc::is_rasl(t)
            && !hevc::is_radl(t)
            && !hevc::is_sub_layer_non_ref(t)
        {
            self.prev_tid0_poc = poc;
        }
        self.first_picture = false;
        self.after_eos = false;
        self.cur = Some(CurPic {
            frame,
            poc,
            sps,
            pps,
            layout,
            output: sh.pic_output_flag,
            pts: self.pending_pts.take(),
            prev_sh: None,
            st_before,
            st_after,
            lt_curr,
            info,
            col,
        });
        Ok(())
    }

    /// Stores a generated reference picture (8.3.3), not needed for output, and returns
    /// its position in the buffer. The caller has reserved its room.
    fn generate_missing(
        &mut self,
        sps: &Sps,
        poc: i32,
        marking: Marking,
        info: Arc<OutInfo>,
    ) -> Result<usize> {
        let (w, h) = (sps.width as usize, sps.height as usize);
        let frame = if sps.high_bit_depth() {
            AnyFrame::U16(Frame::try_new(
                &self.budget,
                w,
                h,
                1u16 << (sps.bit_depth_luma - 1),
            )?)
        } else {
            AnyFrame::U8(Frame::try_new(&self.budget, w, h, 128u8)?)
        };
        let picture = DpbPic {
            frame: SharedFrame::new(frame, &self.pool),
            col: Arc::new(ColMotion::try_new(&self.budget, w, h)?),
            pts: None,
            info,
        };
        let stored = NewPicture {
            poc,
            output: false,
            marking,
            field: None,
            hold: false,
        };
        self.dpb
            .store(stored, picture)
            .map_err(|refused| refused.error)?;
        Ok(self.dpb.len() - 1)
    }

    fn finish_picture(&mut self) -> Result<()> {
        let Some(mut cur) = self.cur.take() else {
            return Ok(());
        };
        let sps = cur.sps.clone();
        let pps = cur.pps.clone();
        // In-loop filters (8.7) for the CTB rows not yet filtered during decoding.
        match &mut cur.frame {
            AnyFrame::U8(f) => {
                let Scratch { band, saved, .. } = &mut self.scratch8;
                filter_ready(f, &mut self.st, &sps, &pps, &cur.layout, band, saved, true);
            }
            AnyFrame::U16(f) => {
                let Scratch { band, saved, .. } = &mut self.scratch16;
                filter_ready(f, &mut self.st, &sps, &pps, &cur.layout, band, saved, true);
            }
        }
        // Collocated motion storage (16x16 granularity): the motion of each block's
        // top-left 4x4 unit, with the order counts of the slice of the block's CTB.
        let st = &self.st;
        // 1, 2 or 4 blocks to a CTB side (log2_ctb is 4 to 6).
        let ctb_blocks = 1 << (sps.log2_ctb - 4);
        let w16 = cur.col.w16;
        for (y, row) in cur.col.data.chunks_exact_mut(w16).enumerate() {
            let motion = &st.motion[y * 4 * st.w4..];
            let ctb_row = (y >> (sps.log2_ctb - 4)) * sps.pic_w_ctb as usize;
            for (cx, blocks) in row.chunks_mut(ctb_blocks).enumerate() {
                let ctb = ctb_row + cx;
                // A CTB no slice of this picture decoded holds an earlier picture's motion.
                if st.ctb_slice_addr[ctb] == u32::MAX {
                    continue;
                }
                let Some(si) = st.slices.get(st.ctb_slice_idx[ctb] as usize) else {
                    continue;
                };
                for (x, out) in (cx * ctb_blocks..).zip(blocks) {
                    let m = motion[x * 4];
                    if !m.is_inter() {
                        continue;
                    }
                    let mut c = ColMv {
                        mv: m.mv,
                        ref_poc: [0; 2],
                        flags: 0,
                    };
                    for l in 0..2 {
                        if m.ref_idx[l] >= 0 {
                            c.flags |= 1 << l;
                            c.ref_poc[l] = si.ref_poc[l][m.ref_idx[l] as usize];
                            if si.ref_lt[l][m.ref_idx[l] as usize] {
                                c.flags |= 4 << l;
                            }
                        }
                    }
                    *out = c;
                }
            }
        }
        let col = Arc::new(cur.col);

        // C.3.4 and C.5.2.3: marking, storage and additional bumping, in the room
        // `start_picture` reserved.
        let picture = DpbPic {
            frame: SharedFrame::new(cur.frame, &self.pool),
            col,
            pts: cur.pts,
            info: cur.info,
        };
        self.dpb
            .hevc_store(
                cur.poc,
                cur.output,
                picture,
                limits(&sps),
                output(&mut self.out),
            )
            .map_err(|refused| refused.error)?;
        Ok(())
    }
}

/// The output limits of the highest temporal sub-layer of `sps` (C.5.2.2).
fn limits(sps: &Sps) -> HevcLimits {
    let highest = sps.max_sub_layers_minus1 as usize;
    HevcLimits::from_sps(
        sps.max_dec_pic_buffering[highest] - 1,
        sps.max_num_reorder[highest],
        sps.max_latency_increase_plus1[highest],
    )
}

/// Queues the pictures the buffer outputs. A picture that leaves is dropped, and its
/// frame returns to the pool once no output picture shows it.
fn output(out: &mut VecDeque<Picture>) -> impl FnMut(Leave<'_, DpbPic>) + '_ {
    |leave| {
        if let Leave::Output { poc, picture } = leave {
            out.push_back(Picture::new(
                poc,
                &picture.frame,
                picture.pts,
                &picture.info,
            ));
        }
    }
}

/// The position of the first stored picture `found` accepts.
fn position(
    dpb: &DecodedPictures<DpbPic>,
    found: impl Fn(&View<'_, DpbPic>) -> bool,
) -> Option<usize> {
    dpb.entries().position(|d| found(&d))
}

/// Deblocking (8.7.2) and SAO (8.7.3) in CTB-row bands, equivalent to the picture-level
/// order of 8.7.1 because each band's deblocked samples are final once the next band's top
/// horizontal edge has been filtered. SAO reads deblocked samples from a band buffer
/// holding the band plus the (saved, pre-SAO) row above and the row below.
///
/// Run the in-loop filters (8.7) on every CTB row that is ready, in order. Deblocking of
/// band r needs bands r and r+1 decoded (band r+1 intra-predicts from unfiltered samples
/// of band r); SAO of band r needs band r+1 deblocked (its first lines are SAO
/// neighbours, and the edge at its top modifies band r). `all` processes every remaining
/// row (end of picture). Filtering a row soon after decoding keeps it in cache.
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_ready<P: Pixel>(
    f: &mut Frame<P>,
    st: &mut PicState,
    sps: &Sps,
    pps: &Pps,
    layout: &Layout,
    band: &mut Option<Frame<P>>,
    saved: &mut [BudgetVec<P>; 3],
    all: bool,
) {
    let nb = sps.pic_h_ctb as usize;
    let wc = sps.pic_w_ctb as usize;
    let ctb = sps.ctb_size as usize;
    let done = |st: &PicState, r: usize| r >= nb || st.row_done[r] as usize >= wc;
    loop {
        let r = st.filt_next;
        if r < nb && (all || (done(st, r) && done(st, r + 1))) {
            if st.any_deblock {
                let offs = deblock::Offsets::new(st, sps);
                deblock::deblock_band(f, st, sps, pps, &offs, r * ctb, (r + 1) * ctb);
            }
            st.filt_next += 1;
            continue;
        }
        let rr = st.sao_next;
        if rr < st.filt_next && (rr + 1 < st.filt_next || st.filt_next == nb) {
            sao_row(f, st, sps, pps, layout, band, saved, rr);
            st.sao_next += 1;
            continue;
        }
        break;
    }
}

/// SAO of CTB row rr. The band buffer receives the deblocked rows of the band plus the
/// saved last line of the previous band and the first line of the next one.
#[allow(clippy::too_many_arguments)]
fn sao_row<P: Pixel>(
    f: &mut Frame<P>,
    st: &PicState,
    sps: &Sps,
    pps: &Pps,
    layout: &Layout,
    band: &mut Option<Frame<P>>,
    saved: &mut [BudgetVec<P>; 3],
    rr: usize,
) {
    let ctb = sps.ctb_size as usize;
    let wc = sps.pic_w_ctb as usize;
    // `prepare_band` sized the band and the saved lines when the picture started.
    let Some(band) = band.as_mut() else {
        return;
    };
    let any_sao = st.sao[rr * wc..(rr + 1) * wc]
        .iter()
        .any(|s| s.type_idx != [0, 0, 0]);
    let mut y0s = [0i32; 3];
    for c in 0..3 {
        let cs = if c == 0 { ctb } else { ctb / 2 };
        let pl = &f.planes[c];
        let (pw, ph, ps) = (pl.width, pl.height, pl.stride);
        let cy0 = rr * cs;
        let cy1 = (cy0 + cs).min(ph);
        if !any_sao {
            // Only the line the next band's SAO reads above itself is needed.
            saved[c][..pw].copy_from_slice(&pl.data[(cy1 - 1) * ps..(cy1 - 1) * ps + pw]);
            continue;
        }
        let bp = &mut band.planes[c];
        let bs = bp.stride;
        if cy0 > 0 {
            bp.data[..pw].copy_from_slice(&saved[c][..pw]);
        }
        let last = if cy1 < ph { cy1 + 1 } else { cy1 };
        for y in cy0..last {
            let br = (y + 1 - cy0) * bs;
            bp.data[br..br + pw].copy_from_slice(&pl.data[y * ps..y * ps + pw]);
        }
        let lr = (cy1 - cy0) * bs;
        saved[c][..pw].copy_from_slice(&bp.data[lr..lr + pw]);
        y0s[c] = cy0 as i32 - 1;
    }
    if any_sao {
        sao::sao_band(f, band, y0s, st, sps, pps, layout, rr as i32);
    }
}

/// Size the SAO band buffer and the saved lines of `sc` for pictures `w` luma samples
/// wide with `ctb`-sample CTBs. They are kept while the geometry stays the same.
fn prepare_band<P: Pixel>(
    sc: &mut Scratch<P>,
    w: usize,
    ctb: usize,
    budget: &Budget,
) -> Result<()> {
    if sc
        .band
        .as_ref()
        .is_some_and(|b| b.planes[0].width == w && b.planes[0].height == ctb + 2)
    {
        return Ok(());
    }
    // Release the old buffers before the new ones are charged.
    sc.band = None;
    sc.saved = core::array::from_fn(|_| BudgetVec::new(budget));
    let chroma = (w / 2, ctb / 2 + 2);
    sc.band = Some(Frame::with_planes(
        budget,
        [(w, ctb + 2), chroma, chroma],
        P::default(),
    )?);
    for (saved, pw) in sc.saved.iter_mut().zip([w, w / 2, w / 2]) {
        *saved = BudgetVec::try_filled(budget, P::default(), pw)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_slice_generic<P: Pixel>(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceHeader,
    layout: &Layout,
    frame: &mut Frame<P>,
    st: &mut PicState,
    dpb: &[View<'_, DpbPic>],
    lists: &[[Option<usize>; 16]; 2],
    sc: &mut Scratch<P>,
    rbsp: &[u8],
    slice_idx: u16,
    poc: i32,
) -> Result<()> {
    let empty = RefEntry {
        frame: None,
        poc: i32::MIN,
        lt: false,
    };
    let mut refs = RefLists {
        list: [[empty; 16]; 2],
        num: [0, 0],
        col: None,
        col_poc: 0,
        no_backward_pred: true,
    };
    let si = &st.slices[slice_idx as usize];
    for l in 0..2 {
        refs.num[l] = sh.num_ref_idx[l] as usize;
        for r in 0..refs.num[l] {
            if let Some(i) = lists[l][r] {
                let d = &dpb[i];
                refs.list[l][r] = RefEntry {
                    frame: P::view(&d.picture().frame),
                    poc: d.poc(),
                    lt: si.ref_lt[l][r],
                };
                if d.poc() > poc {
                    refs.no_backward_pred = false;
                }
            }
        }
    }
    if sh.temporal_mvp && sh.slice_type != SLICE_I {
        let l = if sh.slice_type == SLICE_B && !sh.collocated_from_l0 {
            1
        } else {
            0
        };
        if let Some(i) = lists[l][sh.collocated_ref_idx as usize] {
            refs.col = Some(&dpb[i].picture().col);
            refs.col_poc = dpb[i].poc();
        }
    }
    let mut dec = SliceDecoder::new(
        sps, pps, sh, layout, frame, st, &refs, sc, rbsp, slice_idx, poc,
    )?;
    dec.decode()
}
