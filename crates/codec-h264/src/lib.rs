#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used,
    unreachable_pub,
    missing_docs
)]
// Kept in the forms whose generated code was measured on the hot paths.
#![allow(
    clippy::bool_to_int_with_if,
    clippy::map_unwrap_or,
    clippy::nonminimal_bool,
    clippy::single_match_else,
    clippy::unnested_or_patterns
)]
mod cabac;
mod cavlc;
mod deblock;
mod dpb;
/// Stream feature inspection for classifying conformance vectors (research tooling).
#[cfg(feature = "inspect")]
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
mod tables_vlc;

use dpb::{Current, NewMarking, PicNums, PocState, Stored as _};
use mbinfo::{MbInfo, MbWidth, SliceDbk};
use nal::{parse_header, unescape};
use picture::*;
use ps::{Pps, Sps};
use slice::{SliceHeader, SliceType};
use slicedec::{ColInfo, CurPic, Refs, SliceCtx, SliceDecoder, Wp};
use std::collections::VecDeque;
use tables::*;
use triergon_bitstream::BitReader;
pub use triergon_codec::DecodeError;
use triergon_codec::{Budget, BudgetVec, Colour, PictureView, StorageError, VideoDecoder};
use triergon_framing::nal::AnnexBUnits;
use triergon_framing::picture_output::levels::max_dpb_mbs;
use triergon_framing::picture_output::{
    AvcPicture, DecodedPictures, Leave, Marking, NewPicture, PictureId, View,
};

/// Error of the slice data path, which charges no storage. Without a `Budget` case it keeps
/// every macroblock-level `Result` as small as before the budget existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SliceError {
    Unsupported(&'static str),
    Invalid(&'static str),
}

impl From<triergon_bitstream::cabac::InvalidOffset> for SliceError {
    fn from(_: triergon_bitstream::cabac::InvalidOffset) -> Self {
        SliceError::Invalid("arithmetic code offset out of range")
    }
}

impl From<SliceError> for DecodeError {
    fn from(error: SliceError) -> Self {
        match error {
            SliceError::Unsupported(s) => DecodeError::Unsupported(s),
            SliceError::Invalid(s) => DecodeError::Invalid(s),
        }
    }
}

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Live storage that depends on the stream: frame sample buffers (also while output
    /// pictures share them), motion and macroblock arrays, deblocking state, RBSP and slice
    /// storage. The default holds a level 5.2 decoded picture buffer plus five more frames
    /// of the largest level 5.2 picture and the scratch of one picture, with margin.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 320 * 1024 * 1024,
        }
    }
}

/// Colour description from the VUI (Annex E); defaults are "unspecified" (2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColourDescription {
    /// colour_primaries (Table E-3).
    pub primaries: u8,
    /// transfer_characteristics (Table E-4).
    pub transfer: u8,
    /// matrix_coefficients (Table E-5).
    pub matrix: u8,
    /// video_full_range_flag.
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
    component: Component,
    offset: usize,
    /// Visible samples per row.
    pub width: usize,
    /// Visible rows.
    pub height: usize,
    /// Distance in samples between the starts of two rows.
    pub stride: usize,
}

/// The plane of a frame buffer a [`Plane`] shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Component {
    Y,
    Cb,
    Cr,
}

impl Plane {
    fn samples(&self) -> &[u8] {
        match self.component {
            Component::Y => &self.frame.y,
            Component::Cb => &self.frame.cb,
            Component::Cr => &self.frame.cr,
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
    /// Cropped width in luma samples.
    pub width: usize,
    /// Cropped height in luma samples.
    pub height: usize,
    /// Bits per sample: always 8.
    pub bit_depth: u32,
    /// Y, Cb and Cr; the chroma planes have half the width and height.
    pub planes: [Plane; 3],
    /// The timestamp passed with the access unit that carried the picture.
    pub pts: Option<i64>,
    /// PicOrderCnt of the frame (8.2.1): the smaller of its fields' counts.
    pub poc: i32,
    /// Sample aspect ratio from the VUI as (width, height); (0, 0) when unspecified.
    pub sar: (u32, u32),
    /// Colour description from the VUI.
    pub colour: ColourDescription,
    /// The picture was coded as fields (both fields of the output frame).
    pub interlaced: bool,
}

/// Where the picture being decoded is kept until it is stored.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // One per decoder, moved once per picture.
enum Target {
    /// A frame or first field: the frame store it takes when it is finished.
    New(FrameStore),
    /// A second field: the stored frame store of its first field.
    SecondField(PictureId),
}

#[derive(Debug)]
struct ActivePic {
    pic: CurPic,
    target: Target,
    sps: Sps,
    /// TopFieldOrderCnt and BottomFieldOrderCnt (8.2.1).
    poc: [i32; 2],
    /// PicOrderCntMsb for pic_order_cnt_type 0, FrameNumOffset for the other types.
    poc_base: i32,
    /// The header of the picture's first slice: what later slices are compared with
    /// (7.4.1.2.4) and the reference marking.
    sh: SliceHeader,
}

/// H.264 decoder.
#[derive(Debug)]
pub struct Decoder {
    spss: Vec<Option<Box<Sps>>>,
    ppss: Vec<Option<Box<Pps>>>,
    /// The decoded picture buffer: its frame stores, their marking and their output.
    dpb: DecodedPictures<FrameStore>,
    /// The frame stores not in the buffer, kept with their storage for reuse.
    free: Vec<FrameStore>,
    /// The frame stores made so far, stored, free or being decoded into.
    stores: usize,
    active: Option<ActivePic>,
    out: VecDeque<Picture>,
    /// The picture the last [`VideoDecoder::receive`] view borrows.
    held: Option<Picture>,
    poc_state: PocState,
    max_lt_idx: Option<u32>,
    uid: u32,
    rbsp: BudgetVec<u8>,
    /// The last decoded first field, held from output while it waits for its second
    /// field: its stored picture, field and frame_num.
    pending_field: Option<(PictureId, Structure, u32)>,
    /// Dimensions of the active SPS (to flush on change).
    cur_dims: Option<(u32, u32)>,
    pending_pts: Option<i64>,
    /// Macroblock, slice and deblocking arrays of the last picture, reused by the next.
    pic_scratch: Option<(BudgetVec<MbInfo>, BudgetVec<SliceDbk>, deblock::Progress)>,
    /// The pool every buffer sized from the stream is charged to.
    budget: Budget,
    /// Samples of no picture, for references whose store has none.
    no_samples: FrameBuf,
    /// Prediction scratch of one macroblock (a second field predicting from its first).
    mb_samples: inter::MbSamples,
    /// Sample buffers given up by frame stores while an output picture still shared them;
    /// reused once the caller has dropped that picture (avoids reallocating frames).
    spare_bufs: Vec<std::sync::Arc<FrameBuf>>,
    /// Motion arrays of non-reference pictures (never co-located), kept for reuse.
    spare_motion: Vec<Motion>,
    errors: u64,
}

/// Spare buffers kept for reuse beyond the frame stores.
const MAX_SPARE: usize = 1;

/// Takes the pictures leaving the decoded picture buffer: an output picture is queued
/// (a non-existing frame is not output) and an emptied frame store becomes free.
fn leave<'a>(
    out: &'a mut VecDeque<Picture>,
    free: &'a mut Vec<FrameStore>,
) -> impl FnMut(Leave<'_, FrameStore>) + 'a {
    move |leave| match leave {
        Leave::Output { poc, picture } => out.extend(output_picture(poc, picture)),
        Leave::Removed { picture, .. } => free.push(picture),
    }
}

/// The cropped output picture of a frame store with order count `poc`, sharing its
/// samples; none for a non-existing frame.
fn output_picture(poc: i32, fs: &FrameStore) -> Option<Picture> {
    if fs.non_existing {
        return None;
    }
    let b = fs.buf.as_ref()?;
    let (cx, cy, cw, ch) = fs.info.crop;
    let (cx, cy, cw, ch) = (cx as usize, cy as usize, cw as usize, ch as usize);
    let (ccw, cch) = (cw / 2, ch / 2);
    let mk = |component: Component| -> Plane {
        if component == Component::Y {
            Plane {
                frame: b.clone(),
                component,
                offset: cy * b.width + cx,
                width: cw,
                height: ch,
                stride: b.width,
            }
        } else {
            Plane {
                frame: b.clone(),
                component,
                offset: (cy / 2) * b.cwidth + cx / 2,
                width: ccw,
                height: cch,
                stride: b.cwidth,
            }
        }
    };
    Some(Picture {
        width: cw,
        height: ch,
        bit_depth: 8,
        planes: [mk(Component::Y), mk(Component::Cb), mk(Component::Cr)],
        pts: fs.pts,
        poc,
        sar: fs.info.sar,
        colour: fs.info.colour,
        interlaced: fs.coding == Coding::Fields,
    })
}

/// Moves a decoded picture's samples and motion into its frame store `fs`. Later pictures
/// read only the corner blocks of a reference's motion when `compact` (8.4.1.2.1 with
/// direct_8x8_inference_flag, frame / frame co-location); where the compact copy does not
/// fit, the store keeps the full arrays, which read the same and are charged already. A
/// non-reference picture is never co-located (RefPicList1[0] is a reference), so its
/// motion is kept for reuse.
fn keep_decoded(
    fs: &mut FrameStore,
    pic: CurPic,
    structure: Structure,
    reference: bool,
    compact: bool,
    spare_motion: &mut Vec<Motion>,
) -> (BudgetVec<MbInfo>, BudgetVec<SliceDbk>, deblock::Progress) {
    let slot = usize::from(structure == Structure::Bottom);
    fs.buf = Some(std::sync::Arc::new(pic.buf));
    let spare = if reference && !(compact && fs.motion[slot].compact_from(&pic.motion).is_ok()) {
        std::mem::replace(&mut fs.motion[slot], pic.motion)
    } else {
        pic.motion
    };
    if spare_motion.len() < MAX_SPARE {
        spare_motion.push(spare);
    }
    fs.fields |= if structure == Structure::Frame {
        3
    } else {
        structure as u8
    };
    (pic.mbs, pic.slices, pic.dbk)
}

/// A sample buffer of the given size: a spare one no output picture shares any more, or a new one.
fn recycle_buf(
    spare: &mut Vec<std::sync::Arc<FrameBuf>>,
    w: usize,
    h: usize,
    budget: &Budget,
) -> Result<FrameBuf, StorageError> {
    if let Some(i) = spare
        .iter()
        .position(|b| std::sync::Arc::strong_count(b) == 1 && b.width == w && b.height == h)
        && let Ok(b) = std::sync::Arc::try_unwrap(spare.swap_remove(i))
    {
        return Ok(b);
    }
    FrameBuf::new(w, h, budget)
}

/// Keep `b` (still shared by an output picture) as a future spare.
fn keep_spare(spare: &mut Vec<std::sync::Arc<FrameBuf>>, b: std::sync::Arc<FrameBuf>) {
    if spare.len() >= MAX_SPARE {
        spare.remove(0);
    }
    spare.push(b);
}

/// The frame store `target` decodes into: its own, or the stored first field's.
fn target_store<'a>(
    dpb: &'a mut DecodedPictures<FrameStore>,
    target: &'a mut Target,
) -> Option<&'a mut FrameStore> {
    match target {
        Target::New(fs) => Some(fs),
        Target::SecondField(id) => dpb.picture_mut(*id),
    }
}

/// The DPB size in frames: MaxDpbFrames of the level (A.3.1; a level Table A-1 does not
/// list counts as the largest), or `max_dec_frame_buffering` where the VUI gives it, and
/// at least `max_num_ref_frames`.
fn dpb_frames(sps: &Sps) -> usize {
    let frame_mbs = (sps.pic_width_in_mbs * sps.frame_height_in_mbs()).max(1);
    let level_mbs = max_dpb_mbs(
        sps.profile_idc,
        sps.constraint_flags & 0x10 != 0,
        sps.level_idc,
    )
    .unwrap_or(696_320);
    let mut n = (level_mbs / frame_mbs).clamp(1, 16) as usize;
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

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with its own pool of the default limit.
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`; fails only for a limit
    /// below the pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Decoder {
            spss: vec![None; 32],
            ppss: vec![None; 256],
            dpb: DecodedPictures::new(budget),
            free: Vec::new(),
            stores: 0,
            active: None,
            out: VecDeque::new(),
            held: None,
            poc_state: PocState::default(),
            max_lt_idx: None,
            uid: 0,
            rbsp: BudgetVec::new(budget),
            pending_field: None,
            cur_dims: None,
            pending_pts: None,
            pic_scratch: None,
            budget: budget.clone(),
            no_samples: FrameBuf::empty(budget),
            mb_samples: inter::MbSamples::new(),
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
        for nal in AnnexBUnits::new(data) {
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
        self.dpb.flush(leave(&mut self.out, &mut self.free));
    }

    /// Discontinuity: drop the DPB (parameter sets are kept).
    pub fn reset(&mut self) {
        if let Some(ActivePic {
            target: Target::New(fs),
            ..
        }) = self.active.take()
        {
            self.free.push(fs);
        }
        self.pending_field = None;
        self.dpb.discard(leave(&mut self.out, &mut self.free));
        self.out.clear();
        self.poc_state = PocState::default();
        self.max_lt_idx = None;
    }

    fn decode_nal(&mut self, nal: &[u8]) -> Result<(), DecodeError> {
        let Some(h) = parse_header(nal) else {
            return Err(DecodeError::Invalid("forbidden_zero_bit"));
        };
        match h.nal_unit_type {
            1 | 5 => self.decode_slice_nal(nal, h),
            2..=4 => Err(DecodeError::Unsupported(
                "data partitioning (Extended profile)",
            )),
            7 => {
                self.finish_picture();
                let len = unescape(&nal[1..], &mut self.rbsp)?;
                let mut r = BitReader::with_end(&self.rbsp, len * 8);
                let sps = ps::parse_sps(&mut r)?;
                let id = sps.id as usize;
                self.spss[id] = Some(Box::new(sps));
                Ok(())
            }
            8 => {
                self.finish_picture();
                let len = unescape(&nal[1..], &mut self.rbsp)?;
                let mut r = BitReader::with_end(&self.rbsp, len * 8);
                let pps = ps::parse_pps(&mut r, &self.spss)?;
                let id = pps.id as usize;
                self.ppss[id] = Some(Box::new(pps));
                Ok(())
            }
            // SEI, access unit delimiter, end of sequence, end of stream: a picture boundary.
            6 | 9..=11 => {
                self.finish_picture();
                Ok(())
            }
            // Everything else is skipped, among it the SVC / MVC units (prefix, subset SPS,
            // slice extensions, Annex G/H), so the base view or layer decodes.
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
        let len = unescape(&nal[1..], &mut self.rbsp)?;
        let mut r = BitReader::with_end(&self.rbsp, len * 8);
        let (_, _, pps_id) = slice::peek_pps_id(&mut r.clone())?;
        let pps = self.ppss[pps_id as usize]
            .as_deref()
            .ok_or(DecodeError::Invalid("missing PPS"))?
            .clone();
        let sps = self.spss[pps.sps_id as usize]
            .as_deref()
            .ok_or(DecodeError::Invalid("missing SPS"))?
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
        // New picture detection (7.4.1.2.4); a change of pic_parameter_set_id alone does
        // not start one.
        let new_pic = match &self.active {
            None => true,
            Some(a) => {
                let a = &a.sh;
                a.frame_num != sh.frame_num
                    || a.field_pic != sh.field_pic
                    || (sh.field_pic && a.bottom_field != sh.bottom_field)
                    || (a.nal.nal_ref_idc == 0) != (sh.nal.nal_ref_idc == 0)
                    || (sps.poc_type == 0
                        && (a.poc_lsb != sh.poc_lsb || a.delta_poc_bottom != sh.delta_poc_bottom))
                    || (sps.poc_type == 1 && a.delta_poc != sh.delta_poc)
                    || a.idr != sh.idr
                    || (sh.idr && a.idr && a.idr_pic_id != sh.idr_pic_id)
            }
        };
        if new_pic {
            self.finish_picture();
            self.start_picture(&sps, &sh)?;
        }
        self.decode_slice_data(&sps, &pps, sh, len)
    }

    /// Begin decoding a new picture: frame_num gap handling, POC, frame store allocation.
    fn start_picture(&mut self, sps: &Sps, sh: &SliceHeader) -> Result<(), DecodeError> {
        let dims = (sps.pic_width_in_mbs, sps.frame_height_in_mbs());
        if self.cur_dims.is_some() && self.cur_dims != Some(dims) {
            // Resolution change: output everything and drop the DPB.
            self.close_pending_field();
            self.dpb.flush(leave(&mut self.out, &mut self.free));
        }
        self.cur_dims = Some(dims);
        if !sh.idr {
            // An IDR picture's frame_num starts the count again: no gap to fill.
            self.handle_frame_num_gap(sps, sh)?;
        }
        let structure = sh.structure();
        // The second field of the pending first field: opposite parity, same frame_num; the
        // second field of an IDR frame may be IDR too.
        let first = self
            .pending_field
            .filter(|&(id, par, frame_num)| {
                structure.is_field()
                    && structure != par
                    && frame_num == sh.frame_num
                    && (!sh.idr || self.dpb.entries().any(|e| e.id() == id && e.picture().idr))
            })
            .map(|(id, _, _)| id);
        if first.is_none() {
            self.close_pending_field();
        }
        let (poc, fno) = dpb::compute_poc(sps, sh, &self.poc_state);
        let mut target = match first {
            Some(id) => Target::SecondField(id),
            None => Target::New(self.alloc_store(sps)?),
        };
        let pic = match self.prepare_picture(sps, sh, structure, poc, &mut target) {
            Ok(pic) => pic,
            Err(error) => {
                if let Target::New(fs) = target {
                    self.free.push(fs);
                }
                return Err(error);
            }
        };
        self.active = Some(ActivePic {
            pic,
            target,
            sps: sps.clone(),
            poc,
            poc_base: fno,
            sh: sh.clone(),
        });
        Ok(())
    }

    /// The arrays and samples of the picture `target` decodes into, with its frame store
    /// set up. Everything the picture needs is charged before the store changes, so a
    /// refused second field leaves its first field as it was.
    fn prepare_picture(
        &mut self,
        sps: &Sps,
        sh: &SliceHeader,
        structure: Structure,
        poc: [i32; 2],
        target: &mut Target,
    ) -> Result<CurPic, DecodeError> {
        let w = sps.pic_width_in_mbs as usize;
        let fh = sps.frame_height_in_mbs() as usize;
        let second_field = matches!(target, Target::SecondField(_));
        if !second_field {
            // The buffer's room for the picture once it is finished.
            self.dpb.try_reserve(1)?;
        }
        let h = if structure == Structure::Frame {
            fh
        } else {
            fh / 2
        };
        let (mut mbs, mut slices, mut dbk) = self.pic_scratch.take().unwrap_or_else(|| {
            let budget = &self.budget;
            (
                BudgetVec::new(budget),
                BudgetVec::new(budget),
                deblock::Progress::new(budget),
            )
        });
        mbs.clear();
        mbs.try_resize(w * h, MbInfo::default())?;
        slices.clear();
        dbk.reset(w, h, sh.mbaff)?;
        // Motion arrays of the picture being decoded: a spare set, resized if it must be.
        let mut motion = self
            .spare_motion
            .pop()
            .unwrap_or_else(|| Motion::empty(&self.budget));
        motion.alloc(w * h)?;
        let fs = target_store(&mut self.dpb, target)
            .ok_or(DecodeError::Invalid("first field not stored"))?;
        let mut buf = match fs.buf.take().map(std::sync::Arc::try_unwrap) {
            Some(Ok(b)) => b,
            // Still held by an output picture: the second field needs the first field's samples.
            Some(Err(a)) if second_field => match a.try_clone() {
                Ok(b) => b,
                Err(error) => {
                    fs.buf = Some(a);
                    return Err(error.into());
                }
            },
            Some(Err(a)) => {
                match recycle_buf(&mut self.spare_bufs, a.width, a.height, &self.budget) {
                    Ok(b) => {
                        keep_spare(&mut self.spare_bufs, a);
                        b
                    }
                    Err(error) => {
                        fs.buf = Some(a);
                        return Err(error.into());
                    }
                }
            }
            None => recycle_buf(&mut self.spare_bufs, w * 16, fh * 16, &self.budget)?,
        };
        if second_field {
            fs.poc[structure.index()] = poc[structure.index()];
        } else {
            fs.fields = 0;
            fs.coding = if sh.field_pic {
                Coding::Fields
            } else if sh.mbaff {
                Coding::Mbaff
            } else {
                Coding::Frame
            };
            fs.frame_num = sh.frame_num;
            fs.poc = poc;
            fs.pts = self.pending_pts;
            fs.non_existing = false;
            fs.idr = sh.idr;
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
            if sps.chroma_format_idc == 0 {
                buf.cb.fill(128);
                buf.cr.fill(128);
            }
        }
        Ok(CurPic {
            first_field: None,
            buf,
            motion,
            mbs,
            w: MbWidth::new(w),
            h,
            structure: structure as u8,
            mbaff: sh.mbaff,
            poc,
            slices,
            decoded_mbs: 0,
            chroma: sps.chroma_format_idc != 0,
            dbk,
        })
    }

    /// A free frame store for a picture of `sps`, once the pictures neither needed for
    /// output nor used for reference have left. At most `dpb_frames(sps) + 3` stores are
    /// made (19 at most); while all of them are stored, pictures are output until one is
    /// free.
    fn alloc_store(&mut self, sps: &Sps) -> Result<FrameStore, DecodeError> {
        let w = sps.pic_width_in_mbs as usize * 16;
        let h = sps.frame_height_in_mbs() as usize * 16;
        let most = dpb_frames(sps) + 3;
        self.dpb.remove_unused(leave(&mut self.out, &mut self.free));
        let mut fs = loop {
            let lowest = self
                .free
                .iter()
                .enumerate()
                .min_by_key(|(_, fs)| fs.number)
                .map(|(at, _)| at);
            if let Some(at) = lowest {
                break self.free.swap_remove(at);
            }
            if self.stores < most {
                self.stores += 1;
                break FrameStore::new(self.stores - 1, &self.budget);
            }
            if !self.dpb.bump(leave(&mut self.out, &mut self.free)) {
                // Every store holds a reference picture none of which waits for output.
                return Err(DecodeError::Policy("decoded picture buffer overflow"));
            }
        };
        if fs
            .buf
            .as_ref()
            .is_none_or(|b| b.width != w || b.height != h)
        {
            // The old storage is released before the new buffer is charged.
            fs.buf = None;
            fs.motion = [Motion::empty(&self.budget), Motion::empty(&self.budget)];
            match recycle_buf(&mut self.spare_bufs, w, h, &self.budget) {
                Ok(buf) => fs.buf = Some(std::sync::Arc::new(buf)),
                Err(error) => {
                    self.free.push(fs);
                    return Err(error.into());
                }
            }
        }
        self.uid = self.uid.wrapping_add(1);
        fs.uid = self.uid & 0x1FFF_FFFF;
        Ok(fs)
    }

    /// Frame number gap handling (8.2.5.2): insert "non-existing" short-term frames.
    fn handle_frame_num_gap(&mut self, sps: &Sps, sh: &SliceHeader) -> Result<(), DecodeError> {
        let max = sps.max_frame_num();
        let prev = self.poc_state.prev_ref_frame_num;
        if sh.frame_num == prev || sh.frame_num == (prev + 1) % max {
            return Ok(());
        }
        // Only when a previous reference exists.
        if !self.dpb.entries().any(|e| e.is_reference()) {
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
            let mut fs = self.alloc_store(sps)?;
            let mbs = sps.pic_width_in_mbs as usize * sps.frame_height_in_mbs() as usize;
            if let Err(error) = self
                .dpb
                .try_reserve(1)
                .and_then(|()| fs.motion[0].alloc(mbs))
            {
                self.free.push(fs);
                return Err(error.into());
            }
            let nums = PicNums {
                frame_num: unused,
                max_frame_num: max,
                structure: Structure::Frame,
            };
            let mut entries = dpb::by_number(self.dpb.entries_mut().collect());
            dpb::sliding_window(&mut entries, None, &nums, sps.max_num_ref_frames);
            drop(entries);
            // POC for type != 0 follows frame_num; FrameNumOffset update.
            let fno = if self.poc_state.prev_frame_num > unused {
                self.poc_state
                    .prev_frame_num_offset
                    .wrapping_add(max as i32)
            } else {
                self.poc_state.prev_frame_num_offset
            };
            let poc = if sps.poc_type != 0 {
                let mut fake = SliceHeader {
                    frame_num: unused,
                    idr: false,
                    ..sh.clone()
                };
                fake.nal.nal_ref_idc = 1;
                fake.field_pic = false;
                fake.delta_poc = [0, 0];
                dpb::compute_poc(sps, &fake, &self.poc_state).0
            } else {
                [0, 0]
            };
            fs.fields = 3;
            fs.coding = Coding::Frame;
            fs.frame_num = unused;
            fs.poc = poc;
            fs.non_existing = true;
            fs.pts = None;
            for f in fs.motion[0].mb_flags.iter_mut() {
                *f = MBF_INTRA;
            }
            let stored = NewPicture {
                poc: fs.frame_poc(),
                output: false,
                marking: Marking::ShortTerm,
                field: None,
                hold: false,
            };
            // Cannot fail: the room is reserved above.
            if let Err(refused) = self.dpb.store(stored, fs) {
                self.free.push(refused.picture);
                return Err(refused.error.into());
            }
            self.poc_state.prev_frame_num_offset = fno;
            self.poc_state.prev_frame_num = unused;
            self.poc_state.prev_ref_frame_num = unused;
            unused = (unused + 1) % max;
        }
        Ok(())
    }

    /// The pending first field gets no second field: it is output on its own, as a frame
    /// with one decoded field.
    fn close_pending_field(&mut self) {
        if let Some((id, _, _)) = self.pending_field.take()
            && let Some(mut first) = self.dpb.entry(id)
        {
            first.release();
        }
    }

    #[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
    fn decode_slice_data(
        &mut self,
        sps: &Sps,
        pps: &Pps,
        sh: SliceHeader,
        len: usize,
    ) -> Result<(), DecodeError> {
        let Some(active) = self.active.as_mut() else {
            return Err(DecodeError::Invalid("slice without picture"));
        };
        let pic = &mut active.pic;
        let nums = PicNums {
            frame_num: sh.frame_num,
            max_frame_num: sps.max_frame_num(),
            structure: sh.structure(),
        };
        // The stored frame stores: while a second field is decoded, the first field of its
        // frame is one of them, visible to it.
        let dpb = dpb::by_number(self.dpb.entries().collect());
        pic.first_field = match active.target {
            Target::SecondField(id) => dpb.iter().position(|d| d.id() == id),
            Target::New(_) => None,
        };
        // Reference lists.
        let mut ref_list: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if !sh.slice_type.is_intra() {
            ref_list = dpb::init_ref_lists(&dpb, &nums, sh.slice_type, pic.pic_poc());
            for l in 0..2 {
                let n = sh.num_ref_idx_active[l] as usize;
                if l == 1 && sh.slice_type != SliceType::B {
                    ref_list[1].clear();
                    continue;
                }
                dpb::modify_ref_list(&dpb, &nums, &mut ref_list[l], &sh.ref_mods[l], n)?;
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
            // A list holds short-term and long-term references (a frame's two fields alike).
            ref_list[1].first().map(|r| ColInfo {
                idx: r.idx,
                parity: r.parity,
                short_term: !r.long,
            })
        } else {
            None
        };
        let slice_num = pic.slices.len() as u16;
        if slice_num >= 0xFFFE {
            return Err(DecodeError::Policy("too many slices"));
        }
        pic.slices
            .try_push(SliceDbk {
                disable_idc: sh.disable_deblocking_filter_idc,
                alpha_off: sh.slice_alpha_c0_offset,
                beta_off: sh.slice_beta_offset,
            })
            .map_err(|e| e.error)?;
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
        let stores: Vec<&FrameStore> = dpb.iter().map(View::picture).collect();
        let refs = Refs {
            dpb: &stores,
            no_samples: &self.no_samples,
        };
        let mut dec = SliceDecoder::new(&ctx, pic, refs, &self.rbsp, len, &mut self.mb_samples);
        dec.run().map_err(DecodeError::from)
    }

    /// Finish the active picture: deblock, reference marking (8.2.5), then removal (C.4.4),
    /// storage and bumping (C.4.5).
    fn finish_picture(&mut self) {
        let Some(mut a) = self.active.take() else {
            return;
        };
        // Conceal undecoded macroblocks minimalistically (keep prediction-free content).
        deblock::finish(&mut a.pic);
        let structure = a.sh.structure();
        let is_ref = a.sh.nal.nal_ref_idc != 0;
        let compact = a.sps.frame_mbs_only && a.sps.direct_8x8_inference;
        let mut target = a.target;
        let Some(fs) = target_store(&mut self.dpb, &mut target) else {
            return;
        };
        self.pic_scratch = Some(keep_decoded(
            fs,
            a.pic,
            structure,
            is_ref,
            compact,
            &mut self.spare_motion,
        ));
        let sps = &a.sps;
        let sh = &a.sh;
        let nums = PicNums {
            frame_num: sh.frame_num,
            max_frame_num: sps.max_frame_num(),
            structure,
        };
        let mut mmco5 = false;
        let mut no_output_prior = false;
        // The marking of a frame or first field, stored once marking is done.
        let mut new = NewMarking::default();
        if is_ref {
            let mut entries = dpb::by_number(self.dpb.entries_mut().collect());
            let first = match target {
                Target::SecondField(id) => entries.iter().position(|e| e.id() == id),
                Target::New(_) => None,
            };
            let mut cur = match first {
                Some(at) => Current::Stored(at),
                None => Current::New(&mut new),
            };
            if sh.idr {
                // All other references unused.
                for (i, e) in entries.iter_mut().enumerate() {
                    if Some(i) != first {
                        e.set_marking(None, Marking::Unused);
                    }
                }
                no_output_prior = sh.no_output_of_prior_pics;
                let marking = if sh.long_term_reference_flag {
                    Marking::LongTerm
                } else {
                    Marking::ShortTerm
                };
                cur.mark(&mut entries, structure, marking, Some(0));
                self.max_lt_idx = sh.long_term_reference_flag.then_some(0);
            } else {
                let mut marked_long = false;
                if sh.adaptive_ref_pic_marking {
                    mmco5 =
                        dpb::apply_mmcos(&mut entries, &mut cur, &nums, sh, &mut self.max_lt_idx);
                    marked_long = sh.mmcos.iter().any(|m| m.op == 6);
                } else {
                    let first_is_short = first.is_some_and(|at| {
                        entries[at].mark(structure.opposite()) == Marking::ShortTerm
                    });
                    if !first_is_short {
                        dpb::sliding_window(&mut entries, first, &nums, sps.max_num_ref_frames);
                    }
                }
                if !marked_long {
                    cur.mark(&mut entries, structure, Marking::ShortTerm, None);
                }
            }
        }
        // POC state for the next picture.
        let mut poc = a.poc;
        if mmco5 && let Some(fs) = target_store(&mut self.dpb, &mut target) {
            let temp = match structure {
                Structure::Frame => poc[0].min(poc[1]),
                Structure::Top => poc[0],
                Structure::Bottom => poc[1],
            };
            poc[0] = poc[0].wrapping_sub(temp);
            poc[1] = poc[1].wrapping_sub(temp);
            match structure {
                Structure::Frame => fs.poc = poc,
                Structure::Top => fs.poc[0] = poc[0],
                Structure::Bottom => fs.poc[1] = poc[1],
            }
            // After mmco 5 the picture is treated as having frame_num 0 (7.4.3).
            fs.frame_num = 0;
        }
        if is_ref {
            if mmco5 {
                self.poc_state.prev_poc_msb = 0;
                self.poc_state.prev_poc_lsb = if structure != Structure::Bottom {
                    poc[0]
                } else {
                    0
                };
            } else {
                self.poc_state.prev_poc_msb = a.poc_base;
                self.poc_state.prev_poc_lsb = sh.poc_lsb as i32;
            }
            self.poc_state.prev_ref_frame_num = if mmco5 { 0 } else { sh.frame_num };
        }
        self.poc_state.prev_frame_num_offset = if mmco5 { 0 } else { a.poc_base };
        self.poc_state.prev_frame_num = if mmco5 { 0 } else { sh.frame_num };
        self.poc_state.prev_mmco5 = mmco5;
        // Removal before storage (C.4.4): an IDR picture or mmco 5 outputs or discards every
        // prior picture; a second field does neither.
        let second_field = matches!(target, Target::SecondField(_));
        self.dpb.avc_remove(
            AvcPicture {
                idr: sh.idr && !second_field,
                mmco5: mmco5 && !second_field,
                no_output_of_prior_pics: no_output_prior,
            },
            leave(&mut self.out, &mut self.free),
        );
        // Storage (C.4.5.1, C.4.5.2): every picture waits for output; a first field is
        // held from bumping until its second field joins it or none comes.
        let field = structure.is_field().then(|| structure.parity());
        self.pending_field = None;
        match target {
            Target::New(mut fs) => {
                if let Some(idx) = new.long_term_frame_idx {
                    fs.long_term_frame_idx = idx;
                }
                let stored = NewPicture {
                    poc: fs.frame_poc(),
                    output: true,
                    marking: new.marking,
                    field,
                    hold: field.is_some(),
                };
                // Cannot fail: `prepare_picture` reserved the room.
                match self.dpb.store(stored, fs) {
                    Ok(id) => {
                        if field.is_some() {
                            self.pending_field = Some((id, structure, sh.frame_num));
                        }
                    }
                    Err(refused) => self.free.push(refused.picture),
                }
            }
            Target::SecondField(id) => {
                if let Some(mut first) = self.dpb.entry(id) {
                    let second = NewPicture {
                        poc: first.picture().poc[structure.index()],
                        output: true,
                        marking: first.marking(structure.parity()),
                        field,
                        hold: false,
                    };
                    let output_alone = first.pair(second);
                    debug_assert!(!output_alone, "a first field is held until its second");
                }
            }
        }
        // Bumping (C.4.5.3) while the DPB is over capacity or the reorder depth exceeded.
        let dpb_size = dpb_frames(sps);
        let reorder = sps
            .vui
            .as_ref()
            .filter(|v| v.bitstream_restriction)
            .map(|v| v.max_num_reorder_frames as usize);
        while (self.dpb.len() > dpb_size || reorder.is_some_and(|r| self.dpb.bumpable() > r))
            && self.dpb.bump(leave(&mut self.out, &mut self.free))
        {}
    }

    /// Number of NAL units that failed to decode so far (diagnostics).
    pub fn error_count(&self) -> u64 {
        self.errors
    }
}

/// The session view of the decoder. A received picture is held until the next call, so
/// its frame buffer returns to the spare pool exactly when the inherent API's would.
impl VideoDecoder for Decoder {
    type Error = DecodeError;

    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeError> {
        self.held = None;
        Decoder::send(self, au, pts)
    }
    fn receive(&mut self) -> Option<PictureView<'_>> {
        self.held = self.out.pop_front();
        let picture = self.held.as_ref()?;
        fn plane(p: &Plane) -> triergon_codec::Plane<'_> {
            let (data, stride, offset) = p.raw();
            triergon_codec::Plane::U8 {
                data,
                stride,
                offset,
            }
        }
        Some(PictureView {
            width: picture.width,
            height: picture.height,
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
