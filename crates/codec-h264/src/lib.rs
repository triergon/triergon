//! H.264 / AVC Annex-B video decoder written from Rec. ITU-T H.264 (08/2024).
//!
//! Pure safe Rust, no dependencies. Selected feature matrix: Constrained Baseline, Main and
//! High profiles, 8-bit 4:2:0 and 4:0:0, frames, field pictures (PAFF) and MBAFF, CAVLC and
//! CABAC. Development prototype; not qualified. See `research/d4/h264/STATUS.md`.
#![forbid(unsafe_code)]

mod bitreader;
mod cabac;
mod cavlc;
mod deblock;
mod dpb;
pub mod inspect;
mod inter;
mod intra;
mod mbdec;
mod mbinfo;
mod mc;
mod nal;
mod picture;
mod ps;
mod recon;
mod slice;
mod slicedec;
mod tables;
mod tables_cabac_init;
#[allow(dead_code)]
mod tables_vlc;

use bitreader::BitReader;
use dpb::{PicNums, PocState};
use mbinfo::{MbInfo, SliceDbk};
use nal::{NalIter, parse_header, unescape};
use picture::*;
use ps::{Pps, Sps};
use slice::{SliceHeader, SliceType};
use slicedec::{ColInfo, CurPic, Refs, SliceCtx, SliceDecoder, Wp};
use std::collections::VecDeque;
use tables::*;

pub use nal::NalIter as AnnexBNalIter;

/// Decoder error. The decoder never panics on bad input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A syntactically valid stream uses a feature outside the selected matrix.
    Unsupported(&'static str),
    /// The bitstream violates the specification.
    InvalidData(&'static str),
    /// A resource cap (dimensions, DPB size) refused the stream.
    ResourceLimit(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::InvalidData(s) => write!(f, "invalid data: {s}"),
            DecodeError::ResourceLimit(s) => write!(f, "resource limit: {s}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Colour description from the VUI (Annex E); defaults are "unspecified" (2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourDescription {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl Default for ColourDescription {
    fn default() -> Self {
        ColourDescription {
            primaries: 2,
            transfer: 2,
            matrix: 2,
            full_range: false,
        }
    }
}

/// One 8-bit sample plane of an output picture. The samples are shared with the decoder's
/// frame buffer (no copy); `row(y)` returns the `width` visible samples of row `y`.
#[derive(Clone)]
pub struct Plane {
    frame: std::sync::Arc<FrameBuf>,
    comp: u8,
    offset: usize,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl Plane {
    fn samples(&self) -> &[u8] {
        match self.comp {
            0 => &self.frame.y,
            1 => &self.frame.cb,
            _ => &self.frame.cr,
        }
    }
    /// Visible samples of row `y` (`width` bytes).
    pub fn row(&self, y: usize) -> &[u8] {
        let o = self.offset + y * self.stride;
        &self.samples()[o..o + self.width]
    }
    /// The plane from its first visible sample (rows `stride` apart).
    pub fn data(&self) -> &[u8] {
        &self.samples()[self.offset..]
    }
    /// The whole decoded plane, its stride and the index of the first visible
    /// (cropped) sample: visible sample (x, y) is `raw[offset + y * stride + x]`.
    pub fn raw(&self) -> (&[u8], usize, usize) {
        (self.samples(), self.stride, self.offset)
    }
}

impl std::fmt::Debug for Plane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Plane {{ {}x{} stride {} }}",
            self.width, self.height, self.stride
        )
    }
}

/// A decoded picture in output order (cropped, 8-bit 4:2:0; monochrome streams carry
/// chroma planes filled with 128 like FFmpeg's yuv420p output).
#[derive(Clone, Debug)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub bit_depth: u32,
    pub planes: [Plane; 3],
    pub pts: Option<i64>,
    pub poc: i32,
    pub sar: (u32, u32),
    pub colour: ColourDescription,
    /// The picture was coded as fields (both fields of the output frame).
    pub interlaced: bool,
}

struct ActivePic {
    pic: CurPic,
    sps: Sps,
    idr: bool,
    nal_ref_idc: u8,
    frame_num: u32,
    field_pic: bool,
    bottom: bool,
    poc: [i32; 2],
    frame_num_offset: i32,
    poc_msb: i32,
    poc_lsb: u32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
    pps_id: u32,
    idr_pic_id: u32,
    second_field: bool,
    /// Marking information from the first slice.
    sh_first: SliceHeader,
    max_slice_err: Option<DecodeError>,
}

/// H.264 decoder.
pub struct Decoder {
    spss: Vec<Option<Box<Sps>>>,
    ppss: Vec<Option<Box<Pps>>>,
    dpb: Vec<FrameStore>,
    active: Option<ActivePic>,
    out: VecDeque<Picture>,
    poc_state: PocState,
    max_lt_idx: Option<u32>,
    uid: u32,
    decode_count: u64,
    rbsp: Vec<u8>,
    /// DPB index of the last decoded first field still waiting for its second field.
    pending_field: Option<(usize, u8, u32)>,
    /// Dimensions of the active SPS (to flush on change).
    cur_dims: Option<(u32, u32)>,
    pending_pts: Option<i64>,
    mbs_scratch: Vec<MbInfo>,
    /// Sample buffers given up by frame stores while an output picture still shared them;
    /// reused once the caller has dropped that picture (avoids reallocating frames).
    spare_bufs: Vec<std::sync::Arc<FrameBuf>>,
    /// Motion arrays of non-reference pictures (never co-located), kept for reuse.
    spare_motion: Vec<Motion>,
    errors: u64,
}

/// Spare buffers kept for reuse beyond the frame stores.
const MAX_SPARE: usize = 1;

/// A sample buffer of the given size: a spare one no output picture shares any more, or a new one.
fn recycle_buf(spare: &mut Vec<std::sync::Arc<FrameBuf>>, w: usize, h: usize) -> FrameBuf {
    if let Some(i) = spare
        .iter()
        .position(|b| std::sync::Arc::strong_count(b) == 1 && b.width == w && b.height == h)
        && let Ok(b) = std::sync::Arc::try_unwrap(spare.swap_remove(i))
    {
        return b;
    }
    FrameBuf::new(w, h, true)
}

/// Keep `b` (still shared by an output picture) as a future spare.
fn keep_spare(spare: &mut Vec<std::sync::Arc<FrameBuf>>, b: std::sync::Arc<FrameBuf>) {
    if spare.len() >= MAX_SPARE {
        spare.remove(0);
    }
    spare.push(b);
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Level limits (Table A-1): MaxDpbMbs by level_idc.
fn max_dpb_mbs(level: u8, constraint_set3: bool, profile: u8) -> u32 {
    match level {
        9 => 396,
        10 => 396,
        11 => {
            if constraint_set3 && matches!(profile, 66 | 77 | 88) {
                396
            } else {
                900
            }
        }
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            spss: vec![None; 32],
            ppss: vec![None; 256],
            dpb: Vec::new(),
            active: None,
            out: VecDeque::new(),
            poc_state: PocState::default(),
            max_lt_idx: None,
            uid: 0,
            decode_count: 0,
            rbsp: Vec::new(),
            pending_field: None,
            cur_dims: None,
            pending_pts: None,
            mbs_scratch: Vec::new(),
            spare_bufs: Vec::new(),
            spare_motion: Vec::new(),
            errors: 0,
        }
    }

    /// Feed one Annex-B access unit with its caller timestamp. The picture carried by the
    /// access unit is completed before returning.
    pub fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeError> {
        let r = self.send_nals(au, pts);
        self.finish_picture();
        r
    }

    /// Feed Annex-B data without implying an access unit boundary at the end (picture
    /// boundaries are detected from slice headers, clause 7.4.1.2.4).
    pub fn send_nals(&mut self, data: &[u8], pts: Option<i64>) -> Result<(), DecodeError> {
        self.pending_pts = pts;
        let mut first_err = None;
        for nal in NalIter::new(data) {
            if let Err(e) = self.decode_nal(nal) {
                self.errors += 1;
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Feed a single NAL unit (without start code prefix).
    pub fn send_nal(&mut self, nal: &[u8], pts: Option<i64>) -> Result<(), DecodeError> {
        self.pending_pts = pts;
        let r = self.decode_nal(nal);
        if r.is_err() {
            self.errors += 1;
        }
        r
    }

    /// Pop the next picture in output order.
    pub fn receive(&mut self) -> Option<Picture> {
        self.out.pop_front()
    }

    /// End of stream: output all remaining pictures.
    pub fn flush(&mut self) {
        self.finish_picture();
        self.close_pending_field();
        while self.bump() {}
        for fs in self.dpb.iter_mut() {
            fs.in_use = false;
            fs.mark = [RefMark::None; 2];
            fs.needed_for_output = false;
        }
    }

    /// Discontinuity: drop the DPB (parameter sets are kept).
    pub fn reset(&mut self) {
        self.active = None;
        self.pending_field = None;
        for fs in self.dpb.iter_mut() {
            fs.in_use = false;
            fs.mark = [RefMark::None; 2];
            fs.needed_for_output = false;
        }
        self.out.clear();
        self.poc_state = PocState::default();
        self.max_lt_idx = None;
    }

    fn decode_nal(&mut self, nal: &[u8]) -> Result<(), DecodeError> {
        let Some(h) = parse_header(nal) else {
            return Err(DecodeError::InvalidData("forbidden_zero_bit"));
        };
        match h.nal_unit_type {
            1 | 5 => self.decode_slice_nal(nal, h),
            2..=4 => Err(DecodeError::Unsupported(
                "data partitioning (Extended profile)",
            )),
            7 => {
                self.finish_picture();
                let len = unescape(&nal[1..], &mut self.rbsp);
                let mut r = BitReader::new(&self.rbsp, len);
                let sps = ps::parse_sps(&mut r)?;
                let id = sps.id as usize;
                self.spss[id] = Some(Box::new(sps));
                Ok(())
            }
            8 => {
                self.finish_picture();
                let len = unescape(&nal[1..], &mut self.rbsp);
                let mut r = BitReader::new(&self.rbsp, len);
                let pps = ps::parse_pps(&mut r, &self.spss)?;
                let id = pps.id as usize;
                self.ppss[id] = Some(Box::new(pps));
                Ok(())
            }
            9..=11 => {
                // Access unit delimiter / end of sequence / end of stream: picture boundary.
                self.finish_picture();
                Ok(())
            }
            6 => {
                self.finish_picture();
                Ok(())
            }
            // SVC / MVC NAL units (prefix, subset SPS, slice extensions) are skipped so the
            // base view / base layer decodes (Annex G/H NAL types).
            14 | 15 | 20 | 21 => Ok(()),
            _ => Ok(()),
        }
    }

    fn check_supported(sps: &Sps, pps: &Pps) -> Result<(), DecodeError> {
        if sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
            return Err(DecodeError::Unsupported(
                "bit depth > 8 (High 10 / High 4:2:2 / High 4:4:4)",
            ));
        }
        if sps.chroma_format_idc > 1 {
            return Err(DecodeError::Unsupported("chroma format 4:2:2 / 4:4:4"));
        }
        if sps.qpprime_y_zero_transform_bypass {
            return Err(DecodeError::Unsupported(
                "lossless transform bypass (High 4:4:4)",
            ));
        }
        if pps.num_slice_groups > 1 {
            return Err(DecodeError::Unsupported(
                "flexible macroblock ordering (slice groups)",
            ));
        }
        Ok(())
    }

    fn decode_slice_nal(&mut self, nal: &[u8], h: nal::NalHeader) -> Result<(), DecodeError> {
        let mut rbsp = std::mem::take(&mut self.rbsp);
        let len = unescape(&nal[1..], &mut rbsp);
        let res = self.decode_slice_rbsp(&rbsp, len, h);
        self.rbsp = rbsp;
        res
    }

    fn decode_slice_rbsp(
        &mut self,
        rbsp: &[u8],
        len: usize,
        h: nal::NalHeader,
    ) -> Result<(), DecodeError> {
        let mut r = BitReader::new(rbsp, len);
        let (_, _, pps_id) = slice::peek_pps_id(&mut r.clone())?;
        let pps = self.ppss[pps_id as usize]
            .as_deref()
            .ok_or(DecodeError::InvalidData("missing PPS"))?
            .clone();
        let sps = self.spss[pps.sps_id as usize]
            .as_deref()
            .ok_or(DecodeError::InvalidData("missing SPS"))?
            .clone();
        Self::check_supported(&sps, &pps)?;
        let sh = slice::parse_slice_header(&mut r, h, &sps, &pps)?;
        if matches!(sh.slice_type, SliceType::SP | SliceType::SI) {
            return Err(DecodeError::Unsupported("SP/SI slices (Extended profile)"));
        }
        if sh.redundant_pic_cnt > 0 {
            // Redundant coded pictures are not needed when the primary picture is intact.
            return Ok(());
        }
        // New picture detection (7.4.1.2.4).
        let new_pic = match &self.active {
            None => true,
            Some(a) => {
                a.frame_num != sh.frame_num
                    || a.pps_id != sh.pps_id && false
                    || a.field_pic != sh.field_pic
                    || (sh.field_pic && a.bottom != sh.bottom_field)
                    || (a.nal_ref_idc == 0) != (sh.nal.nal_ref_idc == 0)
                    || (sps.poc_type == 0
                        && (a.poc_lsb != sh.poc_lsb || a.delta_poc_bottom != sh.delta_poc_bottom))
                    || (sps.poc_type == 1 && a.delta_poc != sh.delta_poc)
                    || a.idr != sh.idr
                    || (sh.idr && a.idr && a.idr_pic_id != sh.idr_pic_id)
            }
        };
        if new_pic {
            self.finish_picture();
            self.start_picture(&sps, &pps, &sh)?;
        }
        self.decode_slice_data(&sps, &pps, sh, rbsp, len)
    }

    /// Begin decoding a new picture: frame_num gap handling, POC, frame store allocation.
    fn start_picture(&mut self, sps: &Sps, pps: &Pps, sh: &SliceHeader) -> Result<(), DecodeError> {
        let w = sps.pic_width_in_mbs as usize;
        let fh = sps.frame_height_in_mbs() as usize;
        let dims = (sps.pic_width_in_mbs, sps.frame_height_in_mbs());
        if self.cur_dims.is_some() && self.cur_dims != Some(dims) {
            // Resolution change: output everything and drop the DPB.
            self.close_pending_field();
            while self.bump() {}
            for fs in self.dpb.iter_mut() {
                fs.in_use = false;
                fs.mark = [RefMark::None; 2];
            }
        }
        self.cur_dims = Some(dims);
        let _ = pps;
        if sh.idr {
            // IDR: handled at marking time; frame_num gaps do not apply.
        } else {
            self.handle_frame_num_gap(sps, sh)?;
        }
        let structure: u8 = if !sh.field_pic {
            0
        } else if sh.bottom_field {
            2
        } else {
            1
        };
        // Second field of a pending first field?
        let mut second_field = false;
        let mut store = None;
        if let Some((idx, par, fnum)) = self.pending_field {
            // Opposite parity, same frame_num; the second field of an IDR frame may be IDR too.
            if structure != 0
                && structure != par
                && fnum == sh.frame_num
                && (!sh.idr || self.dpb[idx].idr)
            {
                second_field = true;
                store = Some(idx);
            }
        }
        if !second_field {
            self.close_pending_field();
        }
        let (poc, fno) = dpb::compute_poc(sps, sh, &self.poc_state);
        let store_idx = match store {
            Some(i) => i,
            None => self.alloc_store(sps)?,
        };
        let fs = &mut self.dpb[store_idx];
        if !second_field {
            fs.in_use = true;
            fs.fields = 0;
            fs.mark = [RefMark::None; 2];
            fs.coding = if sh.field_pic {
                Coding::Fields
            } else if sh.mbaff {
                Coding::Mbaff
            } else {
                Coding::Frame
            };
            fs.frame_num = sh.frame_num;
            fs.poc = poc;
            fs.needed_for_output = false;
            fs.pts = self.pending_pts;
            fs.non_existing = false;
            fs.mmco5 = false;
            fs.idr = sh.idr;
            self.decode_count += 1;
            fs.decode_order = self.decode_count;
            let vui = sps.vui.clone().unwrap_or_default();
            fs.info = OutputInfo {
                crop: sps.crop_rect(),
                sar: vui.sar,
                colour: ColourDescription {
                    primaries: vui.colour_primaries,
                    transfer: vui.transfer_characteristics,
                    matrix: vui.matrix_coefficients,
                    full_range: vui.video_full_range,
                },
            };
        } else {
            fs.poc[(structure - 1) as usize] = poc[(structure - 1) as usize];
        }
        let shared = std::mem::take(&mut fs.buf);
        let mut buf = match std::sync::Arc::try_unwrap(shared) {
            Ok(b) => b,
            // Still held by an output picture: the second field needs the first field's samples.
            Err(a) if second_field => (*a).clone(),
            Err(a) => {
                let (bw, bh) = (a.width, a.height);
                let b = recycle_buf(&mut self.spare_bufs, bw, bh);
                keep_spare(&mut self.spare_bufs, a);
                b
            }
        };
        if sps.chroma_format_idc == 0 && !second_field {
            buf.cb.fill(128);
            buf.cr.fill(128);
        }
        let slot = if structure == 2 { 1 } else { 0 };
        // Motion arrays of the picture being decoded: a full-size spare, else the store's own
        // (a corner-compacted store keeps its arrays for its next reference picture).
        let mut motion = match self.spare_motion.pop() {
            Some(m) => m,
            None if !fs.motion[slot].corners => std::mem::take(&mut fs.motion[slot]),
            None => Motion::default(),
        };
        let h = if structure == 0 { fh } else { fh / 2 };
        motion.alloc(w * h);
        let mut mbs = std::mem::take(&mut self.mbs_scratch);
        mbs.clear();
        mbs.resize(w * h, MbInfo::default());
        let pic = CurPic {
            store: store_idx,
            buf,
            motion,
            mbs,
            w,
            h,
            structure,
            mbaff: sh.mbaff,
            poc,
            slices: Vec::new(),
            decoded_mbs: 0,
            chroma: sps.chroma_format_idc != 0,
            dbk: deblock::Progress::new(w, h, sh.mbaff),
        };
        self.active = Some(ActivePic {
            pic,
            sps: sps.clone(),
            idr: sh.idr,
            nal_ref_idc: sh.nal.nal_ref_idc,
            frame_num: sh.frame_num,
            field_pic: sh.field_pic,
            bottom: sh.bottom_field,
            poc,
            frame_num_offset: fno,
            poc_msb: fno,
            poc_lsb: sh.poc_lsb,
            delta_poc_bottom: sh.delta_poc_bottom,
            delta_poc: sh.delta_poc,
            pps_id: sh.pps_id,
            idr_pic_id: sh.idr_pic_id,
            second_field,
            sh_first: sh.clone(),
            max_slice_err: None,
        });
        Ok(())
    }

    fn alloc_store(&mut self, sps: &Sps) -> Result<usize, DecodeError> {
        let w = sps.pic_width_in_mbs as usize * 16;
        let h = sps.frame_height_in_mbs() as usize * 16;
        // Make room: remove pictures neither needed for output nor for reference.
        let max_frames = self.dpb_size(sps) + 2;
        let active = self.active.as_ref().map(|a| a.pic.store);
        let pend = self.pending_field.map(|p| p.0);
        for (i, fs) in self.dpb.iter_mut().enumerate() {
            if Some(i) != active
                && Some(i) != pend
                && fs.in_use
                && !fs.is_ref()
                && !fs.needed_for_output
            {
                fs.in_use = false;
            }
        }
        loop {
            if let Some(i) = self.dpb.iter().position(|f| !f.in_use) {
                let fs = &mut self.dpb[i];
                if fs.buf.width != w || fs.buf.height != h || fs.buf.y.is_empty() {
                    fs.buf = std::sync::Arc::new(recycle_buf(&mut self.spare_bufs, w, h));
                    fs.motion = [Motion::default(), Motion::default()];
                }
                self.uid = self.uid.wrapping_add(1);
                fs.uid = self.uid & 0x1FFF_FFFF;
                return Ok(i);
            }
            if self.dpb.len() < max_frames + 1 {
                self.dpb.push(FrameStore::default());
                continue;
            }
            if !self.bump() {
                // DPB overflow with only reference pictures: evict the oldest non-output ref.
                return Err(DecodeError::ResourceLimit(
                    "decoded picture buffer overflow",
                ));
            }
        }
    }

    fn dpb_size(&self, sps: &Sps) -> usize {
        let frame_mbs = (sps.pic_width_in_mbs * sps.frame_height_in_mbs()).max(1);
        let mut n = (max_dpb_mbs(
            sps.level_idc,
            sps.constraint_flags & 0x10 != 0,
            sps.profile_idc,
        ) / frame_mbs)
            .clamp(1, 16) as usize;
        if let Some(v) = &sps.vui
            && v.bitstream_restriction
        {
            n = (v.max_dec_frame_buffering as usize)
                .max(1)
                .max(sps.max_num_ref_frames as usize)
                .min(16);
        }
        n.max(sps.max_num_ref_frames as usize)
    }

    /// Frame number gap handling (8.2.5.2): insert "non-existing" short-term frames.
    fn handle_frame_num_gap(&mut self, sps: &Sps, sh: &SliceHeader) -> Result<(), DecodeError> {
        let max = sps.max_frame_num();
        let prev = self.poc_state.prev_ref_frame_num;
        if sh.frame_num == prev || sh.frame_num == (prev + 1) % max {
            return Ok(());
        }
        // Only when a previous reference exists.
        if !self.dpb.iter().any(|f| f.in_use && f.is_ref()) {
            return Ok(());
        }
        if !sps.gaps_in_frame_num_allowed {
            // Frame loss: FFmpeg-like concealment is not attempted; proceed without inference.
            return Ok(());
        }
        self.close_pending_field();
        let mut unused = (prev + 1) % max;
        let mut guard = 0;
        while unused != sh.frame_num && guard < max {
            guard += 1;
            let idx = self.alloc_store(sps)?;
            let nums = PicNums {
                frame_num: unused,
                max_frame_num: max,
                structure: 0,
            };
            dpb::sliding_window(&mut self.dpb, idx, &nums, sps.max_num_ref_frames);
            // POC for type != 0 follows frame_num; FrameNumOffset update.
            let fno = if self.poc_state.prev_frame_num > unused {
                self.poc_state.prev_frame_num_offset + max as i32
            } else {
                self.poc_state.prev_frame_num_offset
            };
            let poc = if sps.poc_type != 0 {
                let fake = SliceHeader {
                    frame_num: unused,
                    idr: false,
                    ..sh.clone()
                };
                let mut fake = fake;
                fake.nal.nal_ref_idc = 1;
                fake.field_pic = false;
                fake.delta_poc = [0, 0];
                dpb::compute_poc(sps, &fake, &self.poc_state).0
            } else {
                [0, 0]
            };
            let fs = &mut self.dpb[idx];
            fs.in_use = true;
            fs.fields = 3;
            fs.coding = Coding::Frame;
            fs.mark = [RefMark::Short; 2];
            fs.frame_num = unused;
            fs.poc = poc;
            fs.needed_for_output = false;
            fs.non_existing = true;
            fs.pts = None;
            let mbs = sps.pic_width_in_mbs as usize * sps.frame_height_in_mbs() as usize;
            fs.motion[0].alloc(mbs);
            for f in fs.motion[0].mb_flags.iter_mut() {
                *f = MBF_INTRA;
            }
            self.poc_state.prev_frame_num_offset = fno;
            self.poc_state.prev_frame_num = unused;
            self.poc_state.prev_ref_frame_num = unused;
            unused = (unused + 1) % max;
        }
        Ok(())
    }

    fn close_pending_field(&mut self) {
        if let Some((idx, _, _)) = self.pending_field.take() {
            // Unpaired field: it is output as a frame with one decoded field.
            let _ = idx;
        }
    }

    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    fn decode_slice_data(
        &mut self,
        sps: &Sps,
        pps: &Pps,
        sh: SliceHeader,
        rbsp: &[u8],
        len: usize,
    ) -> Result<(), DecodeError> {
        let Some(active) = self.active.as_mut() else {
            return Err(DecodeError::InvalidData("slice without picture"));
        };
        let pic = &mut active.pic;
        let structure = pic.structure;
        let nums = PicNums {
            frame_num: sh.frame_num,
            max_frame_num: sps.max_frame_num(),
            structure,
        };
        // Reference lists.
        let mut ref_list: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if !sh.slice_type.is_intra() {
            // The first field of the current frame is visible to the second field.
            let cur = pic.store;
            if active.second_field {
                let first = 3 - structure;
                let fs = &mut self.dpb[cur];
                fs.fields = first;
            }
            let lists = dpb::init_ref_lists(&self.dpb, cur, &nums, sh.slice_type, pic.pic_poc());
            ref_list = lists;
            for l in 0..2 {
                let n = sh.num_ref_idx_active[l] as usize;
                if l == 1 && sh.slice_type != SliceType::B {
                    ref_list[1].clear();
                    continue;
                }
                dpb::modify_ref_list(&self.dpb, cur, &nums, &mut ref_list[l], &sh.ref_mods[l], n)?;
            }
        }
        let chroma = sps.chroma_format_idc != 0;
        // Scaling tables.
        let (s4, s8) = if pps.pic_scaling_matrix_present {
            (pps.scaling4, pps.scaling8)
        } else {
            (sps.scaling4, sps.scaling8)
        };
        let mut ls4 = Box::new([[[0i32; 16]; 6]; 6]);
        let mut ls8 = Box::new([[[0i32; 64]; 6]; 2]);
        for i in 0..6 {
            let mut ws = [0i32; 16];
            for k in 0..16 {
                ws[ZIGZAG4[k] as usize] = s4[i][k] as i32;
            }
            for m in 0..6 {
                for p in 0..16 {
                    ls4[i][m][p] = ws[p] * norm_adjust4(m, p / 4, p % 4);
                }
            }
        }
        for i in 0..2 {
            let mut ws = [0i32; 64];
            for k in 0..64 {
                ws[ZIGZAG8[k] as usize] = s8[i][k] as i32;
            }
            for m in 0..6 {
                for p in 0..64 {
                    ls8[i][m][p] = ws[p] * norm_adjust8(m, p / 8, p % 8);
                }
            }
        }
        let wp = match sh.slice_type {
            SliceType::P | SliceType::SP if pps.weighted_pred => Wp::Explicit,
            SliceType::B if pps.weighted_bipred_idc == 1 => Wp::Explicit,
            SliceType::B if pps.weighted_bipred_idc == 2 => Wp::Implicit,
            _ => Wp::Default,
        };
        let mut weights = Box::new([[[(0i32, 0i32); 3]; 32]; 2]);
        let log_wd = [sh.luma_log2_wd, sh.chroma_log2_wd];
        if wp == Wp::Explicit {
            for l in 0..2 {
                for i in 0..32 {
                    let e = sh.weights.as_ref().map(|w| w[l][i]).unwrap_or_default();
                    weights[l][i][0] = e.luma.unwrap_or((1 << sh.luma_log2_wd, 0));
                    weights[l][i][1] = e.chroma[0].unwrap_or((1 << sh.chroma_log2_wd, 0));
                    weights[l][i][2] = e.chroma[1].unwrap_or((1 << sh.chroma_log2_wd, 0));
                }
            }
        }
        let col = if sh.slice_type == SliceType::B {
            ref_list[1].first().map(|r| {
                let fs = &self.dpb[r.idx];
                let short = if r.parity == 0 {
                    fs.is_short_frame()
                } else {
                    fs.mark[(r.parity - 1) as usize] == RefMark::Short
                };
                ColInfo {
                    idx: r.idx,
                    parity: r.parity,
                    short_term: short,
                }
            })
        } else {
            None
        };
        let slice_num = pic.slices.len() as u16;
        if slice_num >= 0xFFFE {
            return Err(DecodeError::ResourceLimit("too many slices"));
        }
        pic.slices.push(SliceDbk {
            disable_idc: sh.disable_deblocking_filter_idc,
            alpha_off: sh.slice_alpha_c0_offset,
            beta_off: sh.slice_beta_offset,
        });
        let ctx = SliceCtx {
            sh,
            slice_num,
            cabac: pps.entropy_coding_mode,
            constrained_intra: pps.constrained_intra_pred,
            t8x8_mode: pps.transform_8x8_mode,
            chroma,
            chroma_qp_off: [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            ls4,
            ls8,
            ref_list,
            direct_8x8_inference: sps.direct_8x8_inference,
            wp,
            weights,
            log_wd,
            col,
        };
        let refs = Refs { dpb: &self.dpb };
        let mut dec = SliceDecoder::new(&ctx, pic, refs, rbsp, len);
        let res = dec.run();
        if let Err(e) = &res {
            active.max_slice_err = Some(e.clone());
        }
        res
    }

    /// Finish the active picture: deblock, marking, DPB storage and output.
    fn finish_picture(&mut self) {
        let Some(mut a) = self.active.take() else {
            return;
        };
        // Conceal undecoded macroblocks minimalistically (keep prediction-free content).
        deblock::finish(&mut a.pic);
        let structure = a.pic.structure;
        let store = a.pic.store;
        let slot = if structure == 2 { 1 } else { 0 };
        {
            let fs = &mut self.dpb[store];
            fs.buf = std::sync::Arc::new(std::mem::take(&mut a.pic.buf));
            if a.nal_ref_idc != 0 {
                if a.sps.frame_mbs_only && a.sps.direct_8x8_inference {
                    // Later pictures only read corner blocks of it (8.4.1.2.1 with
                    // direct_8x8_inference_flag, frame / frame co-location).
                    fs.motion[slot].compact_from(&a.pic.motion);
                    if self.spare_motion.len() < MAX_SPARE {
                        self.spare_motion.push(std::mem::take(&mut a.pic.motion));
                    }
                } else {
                    fs.motion[slot] = std::mem::take(&mut a.pic.motion);
                }
            } else if self.spare_motion.len() < MAX_SPARE {
                // A non-reference picture is never co-located (RefPicList1[0] is a reference).
                self.spare_motion.push(std::mem::take(&mut a.pic.motion));
            }
            fs.fields |= if structure == 0 { 3 } else { structure };
            if a.second_field {
                fs.fields = 3;
            }
        }
        self.mbs_scratch = std::mem::take(&mut a.pic.mbs);
        let sps = &a.sps;
        let sh = &a.sh_first;
        let nums = PicNums {
            frame_num: a.frame_num,
            max_frame_num: sps.max_frame_num(),
            structure,
        };
        let is_ref = a.nal_ref_idc != 0;
        let mut mmco5 = false;
        let mut no_output_prior = false;
        if is_ref {
            if a.idr {
                // All other references unused.
                for (i, fs) in self.dpb.iter_mut().enumerate() {
                    if i != store {
                        fs.mark = [RefMark::None; 2];
                    }
                }
                no_output_prior = sh.no_output_of_prior_pics;
                let fs = &mut self.dpb[store];
                let m = if sh.long_term_reference_flag {
                    RefMark::Long
                } else {
                    RefMark::Short
                };
                if structure == 0 {
                    fs.mark = [m; 2];
                } else {
                    fs.mark[(structure - 1) as usize] = m;
                }
                fs.long_term_frame_idx = 0;
                self.max_lt_idx = if sh.long_term_reference_flag {
                    Some(0)
                } else {
                    None
                };
            } else {
                let mut marked_long = false;
                if sh.adaptive_ref_pic_marking {
                    mmco5 = dpb::apply_mmcos(&mut self.dpb, store, &nums, sh, &mut self.max_lt_idx);
                    marked_long = sh.mmcos.iter().any(|m| m.op == 6);
                } else {
                    let first_is_short = a.second_field && {
                        let fs = &self.dpb[store];
                        fs.mark[(2 - structure) as usize] == RefMark::Short
                    };
                    if !first_is_short {
                        dpb::sliding_window(&mut self.dpb, store, &nums, sps.max_num_ref_frames);
                    }
                }
                if !marked_long {
                    let fs = &mut self.dpb[store];
                    if structure == 0 {
                        fs.mark = [RefMark::Short; 2];
                    } else {
                        fs.mark[(structure - 1) as usize] = RefMark::Short;
                    }
                }
            }
        }
        // POC state for the next picture.
        let mut poc = a.poc;
        if mmco5 {
            let fs = &mut self.dpb[store];
            let temp = match structure {
                0 => poc[0].min(poc[1]),
                1 => poc[0],
                _ => poc[1],
            };
            poc[0] -= temp;
            poc[1] -= temp;
            match structure {
                0 => fs.poc = poc,
                1 => fs.poc[0] = poc[0],
                _ => fs.poc[1] = poc[1],
            }
            fs.mmco5 = true;
            // After mmco 5 the picture is treated as having frame_num 0 (7.4.3).
            fs.frame_num = 0;
        }
        if is_ref {
            if mmco5 {
                self.poc_state.prev_poc_msb = 0;
                self.poc_state.prev_poc_lsb = if structure != 2 { poc[0] } else { 0 };
            } else {
                self.poc_state.prev_poc_msb = a.poc_msb;
                self.poc_state.prev_poc_lsb = a.poc_lsb as i32;
            }
            self.poc_state.prev_ref_frame_num = if mmco5 { 0 } else { a.frame_num };
        }
        self.poc_state.prev_frame_num_offset = if mmco5 { 0 } else { a.frame_num_offset };
        self.poc_state.prev_frame_num = if mmco5 { 0 } else { a.frame_num };
        self.poc_state.prev_mmco5 = mmco5;
        if sps.poc_type == 0 {
            // For POC type 0 frame_num_offset is unused; keep msb in its own slot.
        }
        // Output handling (C.4.4 / C.4.5).
        if (a.idr || mmco5) && !a.second_field {
            {
                if no_output_prior {
                    for (i, fs) in self.dpb.iter_mut().enumerate() {
                        if i != store {
                            fs.in_use = false;
                            fs.needed_for_output = false;
                        }
                    }
                } else {
                    // Output all prior pictures.
                    loop {
                        let cand = self.next_output(Some(store));
                        match cand {
                            Some(i) => self.output(i),
                            None => break,
                        }
                    }
                }
                for (i, fs) in self.dpb.iter_mut().enumerate() {
                    if i != store && !fs.is_ref() && !fs.needed_for_output {
                        fs.in_use = false;
                    }
                }
            }
        }
        let fs = &mut self.dpb[store];
        fs.needed_for_output = true;
        if structure != 0 && !a.second_field {
            self.pending_field = Some((store, structure, a.frame_num));
        } else {
            self.pending_field = None;
        }
        // Remove frames not needed.
        for (i, fs) in self.dpb.iter_mut().enumerate() {
            if i != store && fs.in_use && !fs.is_ref() && !fs.needed_for_output {
                fs.in_use = false;
            }
        }
        // Bumping: output while the DPB is over capacity or reorder depth exceeded.
        let dpb_size = self.dpb_size(sps);
        let reorder = sps
            .vui
            .as_ref()
            .filter(|v| v.bitstream_restriction)
            .map(|v| v.max_num_reorder_frames as usize);
        loop {
            let in_use = self.dpb.iter().filter(|f| f.in_use).count();
            let pend = self.pending_field.map(|p| p.0);
            let waiting = self
                .dpb
                .iter()
                .enumerate()
                .filter(|(i, f)| f.in_use && f.needed_for_output && Some(*i) != pend)
                .count();
            let over = in_use > dpb_size || reorder.map(|r| waiting > r).unwrap_or(false);
            if !over {
                break;
            }
            if !self.bump() {
                break;
            }
        }
    }

    /// Index of the next picture to output (smallest POC among those waiting), skipping the
    /// pending first field and `exclude`.
    fn next_output(&self, exclude: Option<usize>) -> Option<usize> {
        let pend = self.pending_field.map(|p| p.0);
        let mut best: Option<usize> = None;
        for (i, fs) in self.dpb.iter().enumerate() {
            if !fs.in_use || !fs.needed_for_output || Some(i) == exclude {
                continue;
            }
            if Some(i) == pend {
                continue;
            }
            let poc = fs.frame_poc();
            match best {
                None => best = Some(i),
                Some(b) => {
                    let pb = self.dpb[b].frame_poc();
                    if poc < pb || (poc == pb && fs.decode_order < self.dpb[b].decode_order) {
                        best = Some(i);
                    }
                }
            }
        }
        best
    }

    /// Output one picture (C.4.5.3 bumping). Returns false when nothing could be output.
    fn bump(&mut self) -> bool {
        match self.next_output(None) {
            Some(i) => {
                self.output(i);
                let fs = &mut self.dpb[i];
                if !fs.is_ref() {
                    fs.in_use = false;
                    if self.pending_field.map(|p| p.0) == Some(i) {
                        self.pending_field = None;
                    }
                }
                true
            }
            None => false,
        }
    }

    fn output(&mut self, i: usize) {
        let fs = &mut self.dpb[i];
        fs.needed_for_output = false;
        if fs.non_existing {
            return;
        }
        let (cx, cy, cw, ch) = fs.info.crop;
        let (cx, cy, cw, ch) = (cx as usize, cy as usize, cw as usize, ch as usize);
        let b = &fs.buf;
        let (ccw, cch) = (cw / 2, ch / 2);
        let mk = |comp: u8| -> Plane {
            if comp == 0 {
                Plane {
                    frame: b.clone(),
                    comp,
                    offset: cy * b.width + cx,
                    width: cw,
                    height: ch,
                    stride: b.width,
                }
            } else {
                Plane {
                    frame: b.clone(),
                    comp,
                    offset: (cy / 2) * b.cwidth + cx / 2,
                    width: ccw,
                    height: cch,
                    stride: b.cwidth,
                }
            }
        };
        let pic = Picture {
            width: cw,
            height: ch,
            bit_depth: 8,
            planes: [mk(0), mk(1), mk(2)],
            pts: fs.pts,
            poc: fs.frame_poc(),
            sar: fs.info.sar,
            colour: fs.info.colour,
            interlaced: fs.coding == Coding::Fields,
        };
        self.out.push_back(pic);
    }

    /// Number of NAL units that failed to decode so far (diagnostics).
    pub fn error_count(&self) -> u64 {
        self.errors
    }
}
