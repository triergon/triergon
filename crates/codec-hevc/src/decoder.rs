//! Picture-level decoding: NAL dispatch, parameter set activation, POC (8.3.1), RPS (8.3.2),
//! unavailable reference generation (8.3.3), reference picture lists (8.3.4), collocated
//! picture (8.3.5), in-loop filters (8.7) and the output-order DPB (C.5.2).

use std::collections::VecDeque;
use std::sync::Arc;

use crate::ctu::{RefEntry, RefLists, Scratch, SliceDecoder};
use crate::error::{DecodeError, Result};
use crate::nal::{self, NalHeader};
use crate::picture::{AnyFrame, ColMotion, ColMv, Frame, Pixel, Plane as SamplePlane};
use crate::ps::{self, Layout, Pps, Sps};
use crate::slice::{self, SLICE_B, SLICE_I, SliceHeader};
use crate::state::{PicState, SliceInfo};
use crate::{deblock, sao};
use triergon_codec::{Colour, DecodeFailure, PictureView, VideoDecoder};

/// Colour description from the VUI (Annex E). Values 2 mean "unspecified".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourDescription {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
    pub video_signal_type_present: bool,
    pub colour_description_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromaFormat {
    Yuv420,
}

/// Borrowed view of one row of samples.
pub enum Row<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
}

/// One (cropped) sample plane of an output picture. Shares the decoder's frame buffer.
#[derive(Clone)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    frame: Arc<AnyFrame>,
    index: usize,
    x0: usize,
    y0: usize,
}

impl Plane {
    pub fn row(&self, y: usize) -> Row<'_> {
        match &*self.frame {
            AnyFrame::U8(f) => {
                let p = &f.planes[self.index];
                let o = (self.y0 + y) * p.stride + self.x0;
                Row::U8(&p.data[o..o + self.width])
            }
            AnyFrame::U16(f) => {
                let p = &f.planes[self.index];
                let o = (self.y0 + y) * p.stride + self.x0;
                Row::U16(&p.data[o..o + self.width])
            }
        }
    }
    /// Zero-copy view of the whole backing plane: (samples, stride in samples, index of the
    /// first visible (cropped) sample). Sample (x, y) of this plane is
    /// `slice[offset + y * stride + x]` for x < width, y < height.
    pub fn raw(&self) -> (Row<'_>, usize, usize) {
        match &*self.frame {
            AnyFrame::U8(f) => {
                let p = &f.planes[self.index];
                (Row::U8(&p.data), p.stride, self.y0 * p.stride + self.x0)
            }
            AnyFrame::U16(f) => {
                let p = &f.planes[self.index];
                (Row::U16(&p.data), p.stride, self.y0 * p.stride + self.x0)
            }
        }
    }

    /// Append the plane as raw samples (u8, or little-endian u16).
    pub fn write_raw(&self, out: &mut Vec<u8>) {
        for y in 0..self.height {
            match self.row(y) {
                Row::U8(r) => out.extend_from_slice(r),
                Row::U16(r) => {
                    for &v in r {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
        }
    }
}

/// A decoded picture in output order.
#[derive(Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u32,
    pub chroma: ChromaFormat,
    pub planes: [Plane; 3],
    pub pts: Option<i64>,
    pub poc: i32,
    pub sar: (u32, u32),
    pub colour: ColourDescription,
}

/// The session view of the decoder. A received picture is held until the next call, so
/// its frame returns to the pool exactly when the inherent API's would.
impl VideoDecoder for Decoder {
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> core::result::Result<(), DecodeFailure> {
        self.held = None;
        Decoder::send(self, au, pts).map_err(DecodeFailure::from_error)
    }
    fn receive(&mut self) -> Option<PictureView<'_>> {
        self.held = self.out.pop_front();
        let picture = self.held.as_ref()?;
        fn plane(p: &Plane) -> triergon_codec::Plane<'_> {
            match p.raw() {
                (Row::U8(data), stride, offset) => triergon_codec::Plane::U8 {
                    data,
                    stride,
                    offset,
                },
                (Row::U16(data), stride, offset) => triergon_codec::Plane::U16 {
                    data,
                    stride,
                    offset,
                },
            }
        }
        Some(PictureView {
            width: picture.width as usize,
            height: picture.height as usize,
            bit_depth: picture.bit_depth as u8,
            planes: [
                plane(&picture.planes[0]),
                plane(&picture.planes[1]),
                plane(&picture.planes[2]),
            ],
            pts: picture.pts,
            sar: picture.sar,
            colour: Colour {
                matrix: picture.colour.matrix,
                full_range: picture.colour.full_range,
            },
        })
    }
    fn flush(&mut self) {
        self.held = None;
        Decoder::flush(self);
    }
    fn reset(&mut self) {
        self.held = None;
        Decoder::reset(self);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefMark {
    Unused,
    Short,
    Long,
}

struct OutInfo {
    crop: [u32; 4],
    width: u32,
    height: u32,
    bit_depth: u32,
    sar: (u32, u32),
    colour: ColourDescription,
}

struct DpbPic {
    frame: Arc<AnyFrame>,
    col: Arc<ColMotion>,
    poc: i32,
    mark: RefMark,
    needed_for_output: bool,
    latency: u32,
    pts: Option<i64>,
    info: Arc<OutInfo>,
}

struct CurPic {
    frame: AnyFrame,
    poc: i32,
    sps: Arc<Sps>,
    pps: Arc<Pps>,
    layout: Arc<Layout>,
    output: bool,
    pts: Option<i64>,
    prev_sh: Option<SliceHeader>,
    /// DPB indices for RefPicSetStCurrBefore / StCurrAfter / LtCurr.
    st_before: Vec<usize>,
    st_after: Vec<usize>,
    lt_curr: Vec<usize>,
    info: Arc<OutInfo>,
    slices: u16,
}

pub struct Decoder {
    vps: Vec<Option<ps::Vps>>,
    sps: Vec<Option<Arc<Sps>>>,
    pps: Vec<Option<Arc<Pps>>>,
    active_sps: Option<Arc<Sps>>,
    layout_cache: Option<(Arc<Sps>, Arc<Pps>, Arc<Layout>)>,
    dpb: Vec<DpbPic>,
    out: VecDeque<Picture>,
    /// The picture the last [`VideoDecoder::receive`] view borrows.
    held: Option<Picture>,
    cur: Option<CurPic>,
    st: PicState,
    scratch8: Scratch<u8>,
    scratch16: Scratch<u16>,
    pool8: Vec<Frame<u8>>,
    pool16: Vec<Frame<u16>>,
    rbsp: Vec<u8>,
    // POC / IRAP state
    prev_tid0_poc: i32,
    first_picture: bool,
    after_eos: bool,
    no_rasl_output: bool,
    seen_irap: bool,
    pending_pts: Option<i64>,
    pts_used: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            vps: (0..ps::MAX_VPS).map(|_| None).collect(),
            sps: (0..ps::MAX_SPS).map(|_| None).collect(),
            pps: (0..ps::MAX_PPS).map(|_| None).collect(),
            active_sps: None,
            layout_cache: None,
            dpb: Vec::new(),
            out: VecDeque::new(),
            held: None,
            cur: None,
            st: PicState::new(),
            scratch8: Scratch::new(),
            scratch16: Scratch::new(),
            pool8: Vec::new(),
            pool16: Vec::new(),
            rbsp: Vec::new(),
            prev_tid0_poc: 0,
            first_picture: true,
            after_eos: false,
            no_rasl_output: true,
            seen_irap: false,
            pending_pts: None,
            pts_used: false,
        }
    }

    /// Feed one Annex-B access unit with an opaque caller timestamp.
    pub fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<()> {
        self.pending_pts = pts;
        self.pts_used = false;
        let mut result = Ok(());
        for nalu in nal::AnnexBIter::new(au) {
            if let Err(e) = self.decode_nal(nalu) {
                // Abandon the current picture on error but keep the decoder usable.
                if self.cur.is_some() {
                    let _ = self.finish_picture();
                }
                result = Err(e);
                break;
            }
        }
        // One access unit per call: the picture is complete.
        if self.cur.is_some() {
            self.finish_picture()?;
        }
        result
    }

    /// Feed a single NAL unit (without start code). Pictures are completed when the next
    /// picture starts or on `flush`.
    pub fn send_nal(&mut self, nalu: &[u8], pts: Option<i64>) -> Result<()> {
        if pts.is_some() {
            self.pending_pts = pts;
            self.pts_used = false;
        }
        self.decode_nal(nalu)
    }

    /// Pop the next picture in output order, if one is ready.
    pub fn receive(&mut self) -> Option<Picture> {
        self.out.pop_front()
    }

    /// End of stream: output all remaining pictures.
    pub fn flush(&mut self) {
        if self.cur.is_some() {
            let _ = self.finish_picture();
        }
        while self.bump() {}
        self.dpb.clear();
    }

    /// Discontinuity: drop the DPB and any partially decoded picture.
    pub fn reset(&mut self) {
        self.cur = None;
        self.dpb.clear();
        self.out.clear();
        self.first_picture = true;
        self.seen_irap = false;
        self.no_rasl_output = true;
    }

    fn decode_nal(&mut self, nalu: &[u8]) -> Result<()> {
        let Some(h) = NalHeader::parse(nalu) else {
            return Ok(());
        };
        if h.nuh_layer_id > 0 {
            return Ok(());
        }
        let t = h.nal_unit_type;
        match t {
            nal::VPS_NUT => {
                let mut rb = std::mem::take(&mut self.rbsp);
                nal::unescape_rbsp(nalu, &mut rb);
                let r = ps::parse_vps(&rb);
                self.rbsp = rb;
                let v = r?;
                let id = v.id as usize;
                self.vps[id] = Some(v);
            }
            nal::SPS_NUT => {
                let mut rb = std::mem::take(&mut self.rbsp);
                nal::unescape_rbsp(nalu, &mut rb);
                let r = ps::parse_sps(&rb);
                self.rbsp = rb;
                let s = r?;
                let id = s.id as usize;
                self.sps[id] = Some(Arc::new(s));
            }
            nal::PPS_NUT => {
                let mut rb = std::mem::take(&mut self.rbsp);
                nal::unescape_rbsp(nalu, &mut rb);
                let r = ps::parse_pps(&rb);
                self.rbsp = rb;
                let p = r?;
                let id = p.id as usize;
                self.pps[id] = Some(Arc::new(p));
            }
            nal::EOS_NUT | nal::EOB_NUT => {
                if self.cur.is_some() {
                    self.finish_picture()?;
                }
                self.after_eos = true;
            }
            0..=9 | 16..=21 => {
                let mut rb = std::mem::take(&mut self.rbsp);
                nal::unescape_rbsp(nalu, &mut rb);
                let r = self.decode_slice_nal(&rb, h);
                self.rbsp = rb;
                r?;
            }
            _ => {}
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
            if nal::is_irap(t) {
                self.seen_irap = true;
                self.no_rasl_output =
                    nal::is_idr(t) || nal::is_bla(t) || self.first_picture || self.after_eos;
            } else if !self.seen_irap {
                return Ok(());
            }
            if nal::is_rasl(t) && self.no_rasl_output {
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
        if !first {
            let cur = self.cur.as_ref().unwrap();
            if !Arc::ptr_eq(&cur.pps, &pps) && cur.pps.id != pps.id {
                return Err(DecodeError::InvalidData("PPS changed within a picture"));
            }
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
        let cur = self.cur.as_mut().unwrap();
        if !sh.dependent_slice_segment {
            cur.prev_sh = Some(sh.clone());
        }
        let slice_idx = cur.slices;
        cur.slices += 1;
        // Reference picture lists (8.3.4) and SliceInfo.
        let mut info = SliceInfo {
            slice_addr_rs: sh.slice_addr_rs,
            deblocking_disabled: sh.deblocking_disabled,
            beta_offset_div2: sh.beta_offset_div2,
            tc_offset_div2: sh.tc_offset_div2,
            lf_across_slices: sh.loop_filter_across_slices,
            sao_luma: sh.sao_luma,
            sao_chroma: sh.sao_chroma,
            ref_poc: [[i32::MIN; 16]; 2],
            ref_lt: [[false; 16]; 2],
        };
        let mut lists: [[Option<usize>; 16]; 2] = [[None; 16]; 2];
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
                    info.ref_poc[l][r] = self.dpb[e.0].poc;
                    info.ref_lt[l][r] = e.1;
                }
            }
        }
        self.st.slices.push(info);
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
                &self.dpb,
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
                &self.dpb,
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
        sps: Arc<Sps>,
        pps: Arc<Pps>,
    ) -> Result<()> {
        let t = h.nal_unit_type;
        let irap = nal::is_irap(t);
        if let Some(tool) = pps.range_ext_tool
            && sps.profile.profile_idc >= 4
        {
            return Err(DecodeError::Unsupported(tool));
        }
        if pps.scc_ext && sps.profile.profile_idc >= 9 {
            return Err(DecodeError::Unsupported("pps_scc_extension (SCC)"));
        }
        // Activate SPS.
        let sps_changed = match &self.active_sps {
            Some(a) => !Arc::ptr_eq(a, &sps),
            None => true,
        };
        if sps_changed {
            self.active_sps = Some(sps.clone());
        }
        let layout = match &self.layout_cache {
            Some((s, p, l)) if Arc::ptr_eq(s, &sps) && Arc::ptr_eq(p, &pps) => l.clone(),
            _ => {
                let l = Arc::new(Layout::new(&sps, &pps)?);
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
        if h.temporal_id == 0
            && !nal::is_rasl(t)
            && !nal::is_radl(t)
            && !nal::is_sub_layer_non_ref(t)
        {
            self.prev_tid0_poc = poc;
        }
        // RPS (8.3.2)
        if irap && self.no_rasl_output {
            for p in &mut self.dpb {
                p.mark = RefMark::Unused;
            }
        }
        let mut st_before_poc = Vec::new();
        let mut st_after_poc = Vec::new();
        let mut st_foll_poc = Vec::new();
        let mut lt_curr_poc: Vec<(i32, bool)> = Vec::new();
        let mut lt_foll_poc: Vec<(i32, bool)> = Vec::new();
        if !nal::is_idr(t) {
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
            for lt in &sh.long_term {
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
        let find_lt = |dpb: &Vec<DpbPic>, p: i32, msb: bool| -> Option<usize> {
            dpb.iter().position(|d| {
                d.mark != RefMark::Unused
                    && if msb {
                        d.poc == p
                    } else {
                        (d.poc & (max_lsb - 1)) == p
                    }
            })
        };
        let mut lt_curr_idx: Vec<Option<usize>> = Vec::new();
        for &(p, m) in &lt_curr_poc {
            lt_curr_idx.push(find_lt(&self.dpb, p, m));
        }
        let mut lt_foll_idx: Vec<Option<usize>> = Vec::new();
        for &(p, m) in &lt_foll_poc {
            lt_foll_idx.push(find_lt(&self.dpb, p, m));
        }
        for i in lt_curr_idx.iter().chain(lt_foll_idx.iter()).flatten() {
            self.dpb[*i].mark = RefMark::Long;
            keep[*i] = true;
        }
        let find_st = |dpb: &Vec<DpbPic>, p: i32| {
            dpb.iter()
                .position(|d| d.mark == RefMark::Short && d.poc == p)
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
        for i in st_before_idx
            .iter()
            .chain(st_after_idx.iter())
            .chain(st_foll_idx.iter())
            .flatten()
        {
            keep[*i] = true;
        }
        for (i, p) in self.dpb.iter_mut().enumerate() {
            if !keep[i] {
                p.mark = RefMark::Unused;
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
        let highest = sps.max_sub_layers_minus1 as usize;
        if irap && self.no_rasl_output && !self.first_picture {
            let no_output_of_prior = if t == nal::CRA_NUT {
                true
            } else {
                sh.no_output_of_prior_pics
            };
            if no_output_of_prior {
                self.dpb.clear();
            } else {
                while self.bump() {}
                self.dpb.clear();
            }
        } else {
            self.remove_unused();
            loop {
                let needed = self.dpb.iter().filter(|p| p.needed_for_output).count();
                let latency_hit = sps.max_latency_increase_plus1[highest] != 0 && {
                    let max_lat =
                        sps.max_num_reorder[highest] + sps.max_latency_increase_plus1[highest] - 1;
                    self.dpb
                        .iter()
                        .any(|p| p.needed_for_output && p.latency >= max_lat)
                };
                if needed > sps.max_num_reorder[highest] as usize
                    || latency_hit
                    || self.dpb.len() >= sps.max_dec_pic_buffering[highest] as usize
                {
                    if !self.bump() {
                        // Nothing to output: drop the oldest non-reference picture to make room.
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        // The indices computed above are invalid if the DPB changed; recompute by POC.
        let resolve = |dpb: &Vec<DpbPic>, p: i32| {
            dpb.iter()
                .position(|d| d.mark != RefMark::Unused && d.poc == p)
        };
        let mut st_before = Vec::new();
        let mut st_after = Vec::new();
        let mut lt_curr = Vec::new();
        // Generate missing references (8.3.3-style, also used for lost pictures).
        for (k, &p) in st_before_poc.iter().enumerate() {
            let i = match resolve(&self.dpb, p) {
                Some(i) if st_before_idx[k].is_some() => i,
                _ => self.generate_missing(&sps, p, RefMark::Short, info.clone()),
            };
            st_before.push(i);
        }
        for (k, &p) in st_after_poc.iter().enumerate() {
            let i = match resolve(&self.dpb, p) {
                Some(i) if st_after_idx[k].is_some() => i,
                _ => self.generate_missing(&sps, p, RefMark::Short, info.clone()),
            };
            st_after.push(i);
        }
        for (k, &(p, msb)) in lt_curr_poc.iter().enumerate() {
            let found = if lt_curr_idx[k].is_some() {
                self.dpb.iter().position(|d| {
                    d.mark == RefMark::Long
                        && if msb {
                            d.poc == p
                        } else {
                            (d.poc & (max_lsb - 1)) == p
                        }
                })
            } else {
                None
            };
            let i = match found {
                Some(i) => i,
                None => self.generate_missing(&sps, p, RefMark::Long, info.clone()),
            };
            lt_curr.push(i);
        }
        self.first_picture = false;
        self.after_eos = false;
        // Allocate the current frame.
        let (w, hgt) = (sps.width as usize, sps.height as usize);
        let frame = if sps.high_bit_depth() {
            let f = loop {
                match self.pool16.pop() {
                    Some(f) if f.same_geometry(w, hgt) => break f,
                    Some(_) => continue,
                    None => break Frame::<u16>::new(w, hgt, 0),
                }
            };
            AnyFrame::U16(f)
        } else {
            let f = loop {
                match self.pool8.pop() {
                    Some(f) if f.same_geometry(w, hgt) => break f,
                    Some(_) => continue,
                    None => break Frame::<u8>::new(w, hgt, 0),
                }
            };
            AnyFrame::U8(f)
        };
        self.st.reset(w, hgt, sps.pic_size_ctb() as usize);
        self.st.row_done.clear();
        self.st.row_done.resize(sps.pic_h_ctb as usize, 0);
        let pts = if self.pts_used {
            None
        } else {
            self.pending_pts
        };
        self.pts_used = true;
        self.cur = Some(CurPic {
            frame,
            poc,
            sps,
            pps,
            layout,
            output: sh.pic_output_flag,
            pts,
            prev_sh: None,
            st_before,
            st_after,
            lt_curr,
            info,
            slices: 0,
        });
        Ok(())
    }

    fn generate_missing(
        &mut self,
        sps: &Sps,
        poc: i32,
        mark: RefMark,
        info: Arc<OutInfo>,
    ) -> usize {
        let (w, h) = (sps.width as usize, sps.height as usize);
        let frame = if sps.high_bit_depth() {
            AnyFrame::U16(Frame::new(w, h, 1u16 << (sps.bit_depth_luma - 1)))
        } else {
            AnyFrame::U8(Frame::new(w, h, 128u8))
        };
        let w16 = w.div_ceil(16);
        let h16 = h.div_ceil(16);
        self.dpb.push(DpbPic {
            frame: Arc::new(frame),
            col: Arc::new(ColMotion {
                w16,
                h16,
                data: vec![ColMv::default(); w16 * h16],
            }),
            poc,
            mark,
            needed_for_output: false,
            latency: 0,
            pts: None,
            info,
        });
        self.dpb.len() - 1
    }

    fn remove_unused(&mut self) {
        let mut i = 0;
        while i < self.dpb.len() {
            if !self.dpb[i].needed_for_output && self.dpb[i].mark == RefMark::Unused {
                let p = self.dpb.remove(i);
                self.recycle(p.frame);
            } else {
                i += 1;
            }
        }
    }

    fn recycle(&mut self, f: Arc<AnyFrame>) {
        if let Ok(f) = Arc::try_unwrap(f) {
            match f {
                AnyFrame::U8(f) => {
                    if self.pool8.is_empty() {
                        self.pool8.push(f);
                    }
                }
                AnyFrame::U16(f) => {
                    if self.pool16.is_empty() {
                        self.pool16.push(f);
                    }
                }
            }
        }
    }

    /// C.5.2.4 bumping: output the smallest-POC picture needed for output.
    fn bump(&mut self) -> bool {
        let mut best: Option<usize> = None;
        for (i, p) in self.dpb.iter().enumerate() {
            if p.needed_for_output && best.is_none_or(|b| p.poc < self.dpb[b].poc) {
                best = Some(i);
            }
        }
        let Some(i) = best else { return false };
        let p = &mut self.dpb[i];
        p.needed_for_output = false;
        let pic = make_picture(p);
        self.out.push_back(pic);
        if self.dpb[i].mark == RefMark::Unused {
            let p = self.dpb.remove(i);
            self.recycle(p.frame);
        }
        true
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
        // Collocated motion storage (16x16 granularity).
        let w = sps.width as usize;
        let h = sps.height as usize;
        let w16 = w.div_ceil(16);
        let h16 = h.div_ceil(16);
        let mut data = vec![ColMv::default(); w16 * h16];
        for y in 0..h16 {
            for x in 0..w16 {
                let (lx, ly) = (x * 16, y * 16);
                let m = self.st.motion[(ly >> 2) * self.st.w4 + (lx >> 2)];
                if !m.is_inter() {
                    continue;
                }
                let ctb = (ly >> sps.log2_ctb) * sps.pic_w_ctb as usize + (lx >> sps.log2_ctb);
                let Some(si) = self.st.slices.get(self.st.ctb_slice_idx[ctb] as usize) else {
                    continue;
                };
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
                data[y * w16 + x] = c;
            }
        }
        let col = Arc::new(ColMotion { w16, h16, data });

        // C.5.2.3: marking and additional bumping.
        let highest = sps.max_sub_layers_minus1 as usize;
        if cur.output {
            for p in &mut self.dpb {
                if p.needed_for_output && p.poc > cur.poc {
                    p.latency += 1;
                }
            }
        }
        self.dpb.push(DpbPic {
            frame: Arc::new(cur.frame),
            col,
            poc: cur.poc,
            mark: RefMark::Short,
            needed_for_output: cur.output,
            latency: 0,
            pts: cur.pts,
            info: cur.info,
        });
        loop {
            let needed = self.dpb.iter().filter(|p| p.needed_for_output).count();
            let latency_hit = sps.max_latency_increase_plus1[highest] != 0 && {
                let max_lat =
                    sps.max_num_reorder[highest] + sps.max_latency_increase_plus1[highest] - 1;
                self.dpb
                    .iter()
                    .any(|p| p.needed_for_output && p.latency >= max_lat)
            };
            if needed > sps.max_num_reorder[highest] as usize || latency_hit {
                if !self.bump() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(())
    }
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
    saved: &mut [Vec<P>; 3],
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
    saved: &mut [Vec<P>; 3],
    rr: usize,
) {
    let ctb = sps.ctb_size as usize;
    let wc = sps.pic_w_ctb as usize;
    let w = f.planes[0].width;
    let ok = band
        .as_ref()
        .is_some_and(|b| b.planes[0].width == w && b.planes[0].height == ctb + 2);
    if !ok {
        *band = Some(Frame {
            planes: [
                SamplePlane::new(w, ctb + 2, P::default()),
                SamplePlane::new(w / 2, ctb / 2 + 2, P::default()),
                SamplePlane::new(w / 2, ctb / 2 + 2, P::default()),
            ],
        });
    }
    for c in 0..3 {
        let pw = f.planes[c].width;
        if saved[c].len() != pw {
            saved[c].clear();
            saved[c].resize(pw, P::default());
        }
    }
    let band = band.as_mut().unwrap();
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

fn make_picture(p: &DpbPic) -> Picture {
    let i = &p.info;
    let [l, r, t, b] = i.crop;
    let w = (i.width - l - r) as usize;
    let h = (i.height - t - b) as usize;
    let mk = |index: usize, x0: usize, y0: usize, pw: usize, ph: usize| Plane {
        width: pw,
        height: ph,
        frame: p.frame.clone(),
        index,
        x0,
        y0,
    };
    Picture {
        width: w as u32,
        height: h as u32,
        bit_depth: i.bit_depth,
        chroma: ChromaFormat::Yuv420,
        planes: [
            mk(0, l as usize, t as usize, w, h),
            mk(1, l as usize / 2, t as usize / 2, w / 2, h / 2),
            mk(2, l as usize / 2, t as usize / 2, w / 2, h / 2),
        ],
        pts: p.pts,
        poc: p.poc,
        sar: i.sar,
        colour: i.colour,
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_slice_generic<P: Pixel>(
    sps: &Sps,
    pps: &Pps,
    sh: &SliceHeader,
    layout: &Layout,
    frame: &mut Frame<P>,
    st: &mut PicState,
    dpb: &[DpbPic],
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
                    frame: P::view(&d.frame),
                    poc: d.poc,
                    lt: si.ref_lt[l][r],
                };
                if d.poc > poc {
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
            refs.col = Some(&dpb[i].col);
            refs.col_poc = dpb[i].poc;
        }
    }
    let mut dec = SliceDecoder::new(
        sps, pps, sh, layout, frame, st, &refs, sc, rbsp, slice_idx, poc,
    );
    dec.decode()
}
