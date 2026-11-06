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

use bits::BitReader;
use header::{FrameHeader, SetHeader};
use lfe::{Lfe2x, LfeFir, LfeState};
use qmf::{Qmf, QmfState};
use subframe::{HIST, SideInfo, SubBuf, SubframeParams};

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

/// Core audio channel arrangements supported (AMODE 0..=9, Table 5-4): coded channel
/// order mapped to speakers.
fn amode_speakers(amode: u8) -> Option<&'static [Speaker]> {
    use Speaker::*;
    Some(match amode {
        0 => &[FrontCenter],
        1..=4 => &[FrontLeft, FrontRight],
        5 => &[FrontCenter, FrontLeft, FrontRight],
        6 => &[FrontLeft, FrontRight, BackCenter],
        7 => &[FrontCenter, FrontLeft, FrontRight, BackCenter],
        8 => &[FrontLeft, FrontRight, SideLeft, SideRight],
        9 => &[FrontCenter, FrontLeft, FrontRight, SideLeft, SideRight],
        _ => return None,
    })
}

/// Coded slot indices of the front and surround L/R pairs for sum/difference decoding.
fn front_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        2..=4 | 6 | 8 => Some((0, 1)),
        5 | 7 | 9 => Some((1, 2)),
        _ => None,
    }
}

fn surround_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        8 => Some((2, 3)),
        9 => Some((3, 4)),
        _ => None,
    }
}

/// Locate the XCh sync (Table 6-17): 32-bit aligned, at distance XChFSIZE+1 (or, for
/// legacy streams, XChFSIZE) bytes from the end of the frame.
fn find_xch(frame: &[u8], from_bit: usize) -> Option<usize> {
    let end = frame.len() * 8;
    let mut p = from_bit.div_ceil(32) * 32;
    while p + 64 <= end {
        let byte = p / 8;
        if frame[byte..byte + 4] == [0x5A, 0x5A, 0x5A, 0x5A] {
            let mut b = BitReader::new(frame);
            b.seek(p + 32);
            let fs = b.read(10) as usize;
            let dist = frame.len() - byte;
            if dist == fs + 1 || dist == fs {
                return Some(p);
            }
        }
        p += 32;
    }
    None
}

/// First 32-bit aligned occurrence of `sync` at or after `from_bit`.
fn find_sync32(frame: &[u8], from_bit: usize, sync: u32) -> Option<usize> {
    let mut byte = from_bit.div_ceil(32) * 4;
    while byte + 4 <= frame.len() {
        if u32::from_be_bytes([
            frame[byte],
            frame[byte + 1],
            frame[byte + 2],
            frame[byte + 3],
        ]) == sync
        {
            return Some(byte * 8);
        }
        byte += 4;
    }
    None
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

    fn decode_normalized(&mut self, data: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let mut br = BitReader::new(data);
        let h = FrameHeader::parse(&mut br)?;
        if h.fsize > data.len() {
            return Err(DecodeError::InvalidData("frame shorter than FSIZE"));
        }
        let frame = &data[..h.fsize];
        let mut br = {
            let pos = br.position();
            let mut b = BitReader::new(frame);
            b.seek(pos);
            b
        };
        let speakers = amode_speakers(h.amode).ok_or(DecodeError::Unsupported(
            "AMODE above 9 (more than 5 core channels)",
        ))?;
        if h.short != 31 {
            // FFmpeg (reference) refuses deficit-sample frames.
            return Err(DecodeError::Unsupported("deficit samples (SHORT != 31)"));
        }
        let nsubfs = br.read(4) as usize + 1;
        let set = SetHeader::parse(&mut br, h.cpf)?;
        if set.nch != speakers.len() {
            return Err(DecodeError::InvalidData("PCHS does not match AMODE"));
        }
        let cfg = Config {
            amode: h.amode,
            nch: set.nch,
            lff: h.lff,
            filts: h.filts,
            rate: h.sample_rate,
        };
        let same = self.prev == Some(cfg);
        if !same {
            self.reset();
        }
        // FFmpeg (reference) loses the ADPCM and LFE histories whenever a frame needs more
        // subband samples than any frame since the last reset (observed with the
        // `lfe_mixed` / `size_changes_adpcm` synthetic streams; STATUS.md).
        let grows = h.blocks > self.max_blocks;
        if grows {
            self.max_blocks = h.blocks;
            self.lfe_state.reset();
        }
        let keep = same && h.hflag && !grows;
        self.sub.start_frame(0..set.nch, h.blocks, keep);
        self.prev = Some(cfg);

        let params_base = SubframeParams {
            nssc: 0,
            aspf: h.aspf,
            cpf: h.cpf,
            lossless: h.rate == 0x1F,
            dynf: h.dynf,
            lff: h.lff,
        };
        let mut t0 = 0usize;
        let mut lfe_pos = 0usize;
        let mut sf_nssc = [0usize; 16];
        let mut nsubs_all = [0u8; 8];
        nsubs_all[..set.nch].copy_from_slice(&set.nsubs[..set.nch]);
        for sf in 0..nsubfs {
            let nssc = br.read(2) as usize + 1;
            // PSC (partial subsubframe sample count) is read and ignored, as FFmpeg does.
            let _psc = br.read(3);
            if t0 + nssc * 8 > h.blocks {
                return Err(DecodeError::InvalidData("subframes exceed NBLKS"));
            }
            sf_nssc[sf] = nssc;
            let p = SubframeParams {
                nssc,
                ..params_base
            };
            let lfe = if h.lff > 0 {
                Some(&mut self.lfe_dec[lfe_pos..])
            } else {
                None
            };
            subframe::decode_subframe(
                &mut br,
                &set,
                &mut self.side,
                &p,
                &mut self.sub,
                0,
                t0,
                lfe,
                &nsubs_all,
            )?;
            t0 += nssc * 8;
            lfe_pos += 2 * h.lff as usize * nssc;
        }
        if t0 != h.blocks {
            return Err(DecodeError::InvalidData("subframes do not fill NBLKS"));
        }
        // Optional information (Table 5-30). It carries nothing the output depends on, and
        // FFmpeg treats malformed auxiliary data as non-fatal; it only bounds the search for
        // the extensions that follow.
        if h.timef {
            br.skip(32);
        }
        if h.auxf {
            let auxct = br.read(6) as usize;
            br.align(32);
            br.skip(auxct * 8);
        }
        if h.cpf && h.dynf {
            br.skip(16);
        }
        let core_end = if br.overrun() {
            frame.len() * 8
        } else {
            br.position()
        };

        // Core-embedded extensions (clause 6).
        if h.ext_audio && h.ext_audio_id == 6 && find_sync32(frame, core_end, 0x4700_4A03).is_some()
        {
            // XXCH inside the core substream (clause 6.5) is not implemented: no selected
            // vector carries it (every 7.1 vector puts XXCH in the extension substream).
            return Err(DecodeError::Unsupported(
                "XXCH channel extension in the core substream",
            ));
        }
        let mut nslots = set.nch;
        let mut xch = false;
        if h.ext_audio
            && h.ext_audio_id == 0
            && let Some(pos) = find_xch(frame, core_end)
        {
            let mut xb = BitReader::new(frame);
            xb.seek(pos + 32 + 10 + 4);
            let xset = SetHeader::parse(&mut xb, h.cpf)?;
            if xset.nch != 1 {
                return Err(DecodeError::Unsupported("XCh with more than one channel"));
            }
            // History for the XCh slot follows the core's continuity rule.
            nsubs_all[set.nch] = xset.nsubs[0];
            if !self.prev_xch {
                // The Cs synthesis history starts fresh when XCh (re)appears.
                self.qstate[set.nch].reset();
                if let Some(q) = self.q64state.get_mut(set.nch) {
                    q.reset();
                }
            }
            let xkeep = keep && self.prev_xch;
            self.sub.start_frame(set.nch..set.nch + 1, h.blocks, xkeep);
            let mut xt0 = 0;
            for &nssc in &sf_nssc[..nsubfs] {
                let p = SubframeParams {
                    nssc,
                    dynf: false,
                    lff: 0,
                    ..params_base
                };
                subframe::decode_subframe(
                    &mut xb,
                    &xset,
                    &mut self.side,
                    &p,
                    &mut self.sub,
                    set.nch,
                    xt0,
                    None,
                    &nsubs_all,
                )?;
                xt0 += nssc * 8;
            }
            nslots += 1;
            xch = true;
        }
        self.prev_xch = xch;
        let mut have_x96 = false;
        if h.ext_audio
            && h.ext_audio_id == 2
            && let Some(pos) = x96::find(frame, core_end)
        {
            let mut xb = BitReader::new(frame);
            xb.seek(pos + 32);
            if let Some(xh) = x96::X96Header::parse(&mut xb, set.nch, h.cpf)? {
                let buf = self.x96buf.get_or_insert_with(|| SubBuf::new(5, 64));
                buf.start_frame(0..set.nch, h.blocks, keep && self.prev_x96);
                let mut xt0 = 0;
                for &nssc in &sf_nssc[..nsubfs] {
                    x96::decode_subframe(
                        &mut xb,
                        &xh,
                        &mut self.x96side,
                        nssc,
                        h.aspf,
                        h.cpf,
                        h.rate == 0x1F,
                        buf,
                        xt0,
                    )?;
                    xt0 += nssc * 8;
                }
                have_x96 = true;
            }
        }
        if have_x96 != self.prev_x96 {
            // Output rate changes: filter bank histories do not carry over.
            for q in &mut self.qstate {
                q.reset();
            }
            for q in &mut self.q64state {
                q.reset();
            }
            self.lfe2x.reset();
        }
        self.prev_x96 = have_x96;
        let bands = if have_x96 { 64 } else { 32 };
        if have_x96 && self.qmf64.is_none() {
            self.qmf64 = Some(Qmf::new(&tables::X96_FIR, QMF64_GAIN * OUT_SCALE));
            self.q64state = (0..MAX_SLOTS).map(|_| QmfState::new(64)).collect();
        }
        let nsamples = h.blocks * bands;
        let blocks = h.blocks;

        // Histories for the next frame are taken before any in-place post-processing.
        self.sub.save_history(0..nslots, blocks);
        if have_x96 && let Some(xb) = self.x96buf.as_mut() {
            xb.save_history(0..set.nch, blocks);
        }

        // Output plane order (FFmpeg native order) and the plane of every coded slot.
        let mut order: [(Speaker, usize); MAX_SLOTS + 1] = [(Speaker::FrontLeft, 0); MAX_SLOTS + 1];
        let mut n = 0;
        for (ch, &sp) in speakers.iter().enumerate() {
            order[n] = (sp, ch);
            n += 1;
        }
        if xch {
            order[n] = (Speaker::BackCenter, set.nch);
            n += 1;
        }
        const LFE_SRC: usize = usize::MAX;
        if h.lff > 0 {
            order[n] = (Speaker::LowFrequency, LFE_SRC);
            n += 1;
        }
        let order = &mut order[..n];
        order.sort_by_key(|e| e.0);
        let mut plane_of = [0usize; MAX_SLOTS];
        let mut lfe_plane = None;
        out.sample_rate = if have_x96 {
            h.sample_rate * 2
        } else {
            h.sample_rate
        };
        out.samples_per_channel = nsamples;
        out.layout.count = n as u8;
        out.planes.resize_with(n, Vec::new);
        for (i, &(sp, src)) in order.iter().enumerate() {
            out.layout.speakers[i] = sp;
            out.planes[i].resize(nsamples, 0.0);
            if src == LFE_SRC {
                lfe_plane = Some(i);
            } else {
                plane_of[src] = i;
            }
        }

        // Subband-domain post-processing, in place on the synthesis source rows.
        if have_x96 {
            // X96 recombination: core subband samples join the residual's lower 32 bands
            // (no saturation, observed `x96_s58` / `x96_s60`).
            let xbuf = self
                .x96buf
                .as_mut()
                .ok_or(DecodeError::InvalidData("X96 buffer"))?;
            for ch in 0..set.nch {
                for b in 0..32 {
                    let core_row = self.sub.row(ch, b);
                    let row = xbuf.row_mut(ch, b);
                    for t in HIST..HIST + blocks {
                        row[t] += core_row[t];
                    }
                }
            }
        }
        {
            let mut cs_rows = [[0f32; subframe::MAX_T]; 32];
            if xch {
                for (b, dst) in cs_rows.iter_mut().enumerate() {
                    dst[..blocks].copy_from_slice(&self.sub.row(set.nch, b)[HIST..HIST + blocks]);
                }
            }
            let x96buf = self.x96buf.as_mut();
            let (src, sbands): (&mut SubBuf, usize) = match x96buf {
                Some(xb) if have_x96 => (xb, 64),
                _ => (&mut self.sub, 32),
            };
            if xch {
                // XCh carries Cs; the core surrounds hold Ls + 0.7071 Cs and
                // Rs + 0.7071 Cs (clause 6.4.1). Remove the embedded down-mix.
                if let Some((l, r)) = surround_pair(h.amode) {
                    for (b, cs) in cs_rows.iter().enumerate() {
                        for (pair_ch, t) in [l, r].into_iter().zip([0, 0]) {
                            let _ = t;
                            let row = &mut src.row_mut(pair_ch, b)[HIST..HIST + blocks];
                            for (v, &c) in row.iter_mut().zip(cs.iter()) {
                                *v -= c * core::f32::consts::FRAC_1_SQRT_2;
                            }
                        }
                    }
                }
            }
            let fpair = if h.sumf || h.amode == 3 {
                front_pair(h.amode)
            } else {
                None
            };
            let spair = if h.sums { surround_pair(h.amode) } else { None };
            for (l, r) in [fpair, spair].into_iter().flatten() {
                for b in 0..sbands {
                    let mut rr = [0f32; subframe::MAX_T];
                    rr[..blocks].copy_from_slice(&src.row(r, b)[HIST..HIST + blocks]);
                    let mut ll = [0f32; subframe::MAX_T];
                    ll[..blocks].copy_from_slice(&src.row(l, b)[HIST..HIST + blocks]);
                    for (t, v) in src.row_mut(l, b)[HIST..HIST + blocks]
                        .iter_mut()
                        .enumerate()
                    {
                        *v = ll[t] + rr[t];
                    }
                    for (t, v) in src.row_mut(r, b)[HIST..HIST + blocks]
                        .iter_mut()
                        .enumerate()
                    {
                        *v = ll[t] - rr[t];
                    }
                }
            }
        }

        // Synthesis: transpose each slot to time-major blocks and run the filter bank
        // straight into its output plane.
        let qmf = if have_x96 {
            self.qmf64
                .as_ref()
                .ok_or(DecodeError::InvalidData("X96 filter bank"))?
        } else if h.filts {
            &self.qmf_pr
        } else {
            &self.qmf_np
        };
        if self.tbuf.len() < blocks * bands {
            self.tbuf.resize(blocks * bands, 0.0);
        }
        for ch in 0..nslots {
            let (srcbuf, sb): (&SubBuf, usize) = match self.x96buf.as_ref() {
                Some(xb) if have_x96 && ch < set.nch => (xb, 64),
                _ => (&self.sub, 32),
            };
            let tb = &mut self.tbuf[..blocks * bands];
            if sb < bands {
                tb.iter_mut().for_each(|v| *v = 0.0);
            }
            for b in 0..sb {
                let row = &srcbuf.row(ch, b)[HIST..HIST + blocks];
                for (t, &v) in row.iter().enumerate() {
                    tb[t * bands + b] = v;
                }
            }
            let st = if have_x96 {
                &mut self.q64state[ch]
            } else {
                &mut self.qstate[ch]
            };
            let plane = &mut out.planes[plane_of[ch]];
            for t in 0..blocks {
                qmf.synth(
                    st,
                    &mut tb[t * bands..t * bands + bands],
                    &mut plane[t * bands..t * bands + bands],
                );
            }
        }
        if let Some(lp) = lfe_plane {
            let factor = if h.lff == 1 { 128 } else { 64 };
            let n48 = blocks * 32;
            let ndec = n48 / factor;
            let plane = &mut out.planes[lp];
            if have_x96 {
                if self.lfe48.len() < n48 {
                    self.lfe48.resize(n48, 0.0);
                }
                self.lfe_state.interpolate(
                    &self.lfe_fir,
                    factor,
                    &self.lfe_dec[..ndec],
                    &mut self.lfe48[..n48],
                );
                self.lfe2x
                    .interpolate(&self.lfe48[..n48], &mut plane[..nsamples]);
            } else {
                self.lfe_state.interpolate(
                    &self.lfe_fir,
                    factor,
                    &self.lfe_dec[..ndec],
                    &mut plane[..nsamples],
                );
            }
        }

        Ok(())
    }
}
