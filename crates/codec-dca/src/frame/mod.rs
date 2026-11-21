//! Frame decoding stages: core frame and channel set headers (Tables 5-1, 5-21), core
//! subframes, optional information, and the core-embedded XCh / X96 extensions
//! (clause 6), followed by output synthesis.

mod synthesis;

use crate::bits::BitReader;
use crate::header::{FrameHeader, SetHeader};
use crate::lfe::{Lfe2x, LfeFir, LfeState};
use crate::qmf::{Qmf, QmfState};
use crate::subframe::{self, SideInfo, SubBuf, SubframeParams};
use crate::{AudioFrame, DecodeError, Speaker, pr_table, tables, x96};
use triergon_codec::{Budget, BudgetVec, Charged, Reservation, StorageError};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Config {
    amode: u8,
    nch: usize,
    lff: u8,
    filts: bool,
    rate: u32,
}

/// 32-band synthesis history of one slot.
type History32 = QmfState<512>;

/// The X96 64-band filter bank, created when a stream first carries X96.
#[derive(Debug)]
struct X96Bank {
    qmf: Qmf<1024>,
    /// One history per coded slot.
    state: BudgetVec<QmfState<1024>>,
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
    pub(crate) fn new(budget: &Budget) -> Self {
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

/// Per-frame values shared by the decoding stages.
struct CoreFrame<'a> {
    h: FrameHeader,
    set: SetHeader,
    /// The frame bytes (FSIZE).
    data: &'a [u8],
    /// Coded channel order of the core channels.
    speakers: &'static [Speaker],
    nsubfs: usize,
    /// Subsubframe count of each subframe.
    sf_nssc: [usize; 16],
    /// The subband history of the previous frame continues.
    keep: bool,
    params: SubframeParams,
}

impl FrameDecoder {
    pub(crate) fn decode_normalized(
        &mut self,
        data: &[u8],
        out: &mut AudioFrame,
    ) -> Result<(), DecodeError> {
        let (mut f, mut br) = self.begin_frame(data)?;
        let mut nsubs_all = [0u8; 8];
        nsubs_all[..f.set.nch].copy_from_slice(&f.set.nsubs[..f.set.nch]);
        self.decode_core(&mut br, &mut f, &nsubs_all)?;
        let core_end = optional_info(&mut br, &f);

        // Core-embedded extensions (clause 6).
        if f.h.ext_audio
            && f.h.ext_audio_id == 6
            && aligned_syncs(f.data, core_end, XXCH_SYNC, 4)
                .next()
                .is_some()
        {
            // XXCH inside the core substream (clause 6.5) is not implemented: no selected
            // vector carries it (every 7.1 vector puts XXCH in the extension substream).
            return Err(DecodeError::Unsupported(
                "XXCH channel extension in the core substream",
            ));
        }
        let mut nslots = f.set.nch;
        let mut xch = false;
        if f.h.ext_audio
            && f.h.ext_audio_id == 0
            && let Some(pos) = find_xch(f.data, core_end)
        {
            self.decode_xch(&f, pos, &mut nsubs_all)?;
            nslots += 1;
            xch = true;
        }
        self.prev_xch = xch;
        let mut have_x96 = false;
        if f.h.ext_audio
            && f.h.ext_audio_id == 2
            && let Some(pos) = find_x96(f.data, core_end)
        {
            have_x96 = self.decode_x96(&f, pos)?;
        }

        // Storage is sized before the first history update, so a refused charge drops
        // the frame like any other failure.
        let (plane_of, lfe_plane) = self.output_layout(out, &f, xch, have_x96)?;
        self.prepare_synthesis(&f, nslots, have_x96)?;
        self.postprocess_subbands(&f, xch, have_x96)?;
        self.synthesize(out, &f, nslots, have_x96, &plane_of)?;
        if let Some(lp) = lfe_plane {
            self.synthesize_lfe(out, lp, &f, have_x96);
        }

        Ok(())
    }

    /// Frame header and primary audio coding header, and the history continuity rule.
    #[inline(always)]
    fn begin_frame<'a>(
        &mut self,
        data: &'a [u8],
    ) -> Result<(CoreFrame<'a>, BitReader<'a>), DecodeError> {
        let mut br = BitReader::new(data);
        let h = FrameHeader::parse(&mut br)?;
        if h.fsize > data.len() {
            return Err(DecodeError::Invalid("frame shorter than FSIZE"));
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
            return Err(DecodeError::Invalid("PCHS does not match AMODE"));
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
        // `lfe_mixed` / `size_changes_adpcm` synthetic streams).
        let grows = h.blocks > self.max_blocks;
        if grows {
            self.max_blocks = h.blocks;
            self.lfe_state.reset();
        }
        let keep = same && h.hflag && !grows;
        self.prev = Some(cfg);
        self.sub.start_frame(0..set.nch, h.blocks, keep)?;

        let params = SubframeParams {
            nssc: 0,
            aspf: h.aspf,
            cpf: h.cpf,
            lossless: h.rate == 0x1F,
            dynf: h.dynf,
            lff: h.lff,
        };
        let f = CoreFrame {
            h,
            set,
            data: frame,
            speakers,
            nsubfs,
            sf_nssc: [0; 16],
            keep,
            params,
        };
        Ok((f, br))
    }

    /// Core subframes of the primary channel set, with the decimated LFE samples.
    #[inline(always)]
    fn decode_core(
        &mut self,
        br: &mut BitReader<'_>,
        f: &mut CoreFrame<'_>,
        nsubs_all: &[u8; 8],
    ) -> Result<(), DecodeError> {
        let h = &f.h;
        let params_base = &f.params;
        let mut t0 = 0usize;
        let mut lfe_pos = 0usize;
        for sf in 0..f.nsubfs {
            let nssc = br.read(2) as usize + 1;
            // PSC (partial subsubframe sample count) is read and ignored, as FFmpeg does.
            let _psc = br.read(3);
            if t0 + nssc * 8 > h.blocks {
                return Err(DecodeError::Invalid("subframes exceed NBLKS"));
            }
            f.sf_nssc[sf] = nssc;
            let p = SubframeParams {
                nssc,
                ..*params_base
            };
            let lfe = if h.lff > 0 {
                Some(&mut self.lfe_dec[lfe_pos..])
            } else {
                None
            };
            subframe::decode_subframe(
                br,
                &f.set,
                &mut self.side,
                &p,
                &mut self.sub,
                0,
                t0,
                lfe,
                nsubs_all,
            )?;
            t0 += nssc * 8;
            lfe_pos += 2 * h.lff as usize * nssc;
        }
        if t0 != h.blocks {
            return Err(DecodeError::Invalid("subframes do not fill NBLKS"));
        }
        Ok(())
    }

    /// XCh extension (clause 6.4): one extra channel decoded into the slot after the core.
    #[inline(always)]
    fn decode_xch(
        &mut self,
        f: &CoreFrame<'_>,
        pos: usize,
        nsubs_all: &mut [u8; 8],
    ) -> Result<(), DecodeError> {
        let (h, set) = (&f.h, &f.set);
        let mut xb = BitReader::new(f.data);
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
            if let Some(bank) = &mut self.x96 {
                bank.state[set.nch].reset();
            }
        }
        let xkeep = f.keep && self.prev_xch;
        self.sub
            .start_frame(set.nch..set.nch + 1, h.blocks, xkeep)?;
        let mut xt0 = 0;
        for &nssc in &f.sf_nssc[..f.nsubfs] {
            let p = SubframeParams {
                nssc,
                dynf: false,
                lff: 0,
                ..f.params
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
                nsubs_all,
            )?;
            xt0 += nssc * 8;
        }
        Ok(())
    }

    /// X96 extension (clause 6.2.4): the residual of a 64-band synthesis at twice the core
    /// rate. Returns whether the frame carries one.
    #[inline(always)]
    fn decode_x96(&mut self, f: &CoreFrame<'_>, pos: usize) -> Result<bool, DecodeError> {
        let (h, set) = (&f.h, &f.set);
        let mut xb = BitReader::new(f.data);
        xb.seek(pos + 32);
        let Some(xh) = x96::X96Header::parse(&mut xb, set.nch, h.cpf)? else {
            return Ok(false);
        };
        let buf = self
            .x96buf
            .get_or_insert_with(|| SubBuf::new(&self.budget, 5, 64));
        buf.start_frame(0..set.nch, h.blocks, f.keep && self.prev_x96)?;
        let mut xt0 = 0;
        for &nssc in &f.sf_nssc[..f.nsubfs] {
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
        Ok(true)
    }
}

/// Optional information (Table 5-30). It carries nothing the output depends on, and
/// FFmpeg treats malformed auxiliary data as non-fatal; it only bounds the search for
/// the extensions that follow. Returns the bit position where the core data ends.
#[inline(always)]
fn optional_info(br: &mut BitReader<'_>, f: &CoreFrame<'_>) -> usize {
    let h = &f.h;
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
    if br.overrun() {
        f.data.len() * 8
    } else {
        br.position()
    }
}

/// Core audio channel arrangements supported (AMODE 0..=9, Table 5-4): coded channel
/// order mapped to speakers.
fn amode_speakers(amode: u8) -> Option<&'static [Speaker]> {
    use Speaker::{BackCenter, FrontCenter, FrontLeft, FrontRight, SideLeft, SideRight};
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

/// XCh sync word (Table 6-17).
const XCH_SYNC: u32 = 0x5A5A_5A5A;
/// XXCh sync word (clause 6.5).
const XXCH_SYNC: u32 = 0x4700_4A03;

/// Bit positions of the 32-bit aligned occurrences of `sync` at or after `from_bit` that
/// leave at least `room` bytes, the sync word included, in `frame`.
fn aligned_syncs(
    frame: &[u8],
    from_bit: usize,
    sync: u32,
    room: usize,
) -> impl Iterator<Item = usize> + '_ {
    (from_bit.div_ceil(32) * 4..)
        .step_by(4)
        .take_while(move |&byte| byte + room <= frame.len())
        .filter(move |&byte| frame[byte..byte + 4] == sync.to_be_bytes())
        .map(|byte| byte * 8)
}

/// The XCh sync whose distance to the frame end is XChFSIZE + 1 (or, for legacy
/// streams, XChFSIZE) bytes.
fn find_xch(frame: &[u8], from_bit: usize) -> Option<usize> {
    aligned_syncs(frame, from_bit, XCH_SYNC, 8).find(|&p| {
        let mut b = BitReader::new(frame);
        b.seek(p + 32);
        let fs = b.read(10) as usize;
        let dist = frame.len() - p / 8;
        dist == fs + 1 || dist == fs
    })
}

/// The X96 sync (SYNC96) whose distance to the frame end is FSIZE96 bytes.
fn find_x96(frame: &[u8], from_bit: usize) -> Option<usize> {
    aligned_syncs(frame, from_bit, x96::X96_SYNC, 8).find(|&p| {
        let mut b = BitReader::new(frame);
        b.seek(p + 32);
        let fsize96 = b.read(12) as usize + 1;
        frame.len() - p / 8 == fsize96
    })
}
