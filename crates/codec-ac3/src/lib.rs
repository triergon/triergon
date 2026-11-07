#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr
)]
// Pending: `missing_docs`, `clippy::unwrap_used`, `clippy::expect_used`.
// Channel/bin loops are written with explicit indices to mirror the A/52 pseudo code.
#![allow(clippy::needless_range_loop)]
// Pending: these rewrites change the generated code of measured hot paths and are
// adopted with an A/B timing run.
#![allow(clippy::single_match_else)]
mod bitalloc;
mod error;
mod header;
mod imdct;
mod layout;
mod noise;
mod quant;
mod spec_tables;
mod substream;
mod tables;

pub use error::{DecodeError, Feature};
pub use header::{FrameHeader, StreamType, Syntax, access_unit_len, parse_header};
pub use layout::{MAX_CHANNELS, layout_name};
#[doc(hidden)]
pub use substream::{BlockTrace, FrameTrace, Trace};
pub use substream::{DownmixCodes, FrameFeatures, NoiseMode};
pub use triergon_bitstream::crc::crc16_a52 as crc16;
pub use triergon_codec::{AudioFrame, ChannelLayout, Speaker};

use quant::GroupTables;
use substream::{BlockSink, LFE, NCH, PlaneMap, Substream, acmod_channels};
use triergon_bitstream::BitReader;
use triergon_codec::{AudioDecoder, DecodeFailure, DownmixLevels, SurroundEncoding};

/// Bytes of zero padding kept after a frame in the decode buffer.
pub(crate) const PAD: usize = 16;
/// Largest syncframe: 2048 16-bit words (E-AC-3 frmsiz) / 1920 words (AC-3 32 kHz 640 kbps).
const MAX_FRAME_BYTES: usize = 4096;
/// Maximum syncframes per access unit (independent + 8 dependents + other programs).
const MAX_FRAMES_PER_AU: usize = 64;

/// What to do with syncframes whose CRC does not check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CrcPolicy {
    /// Decode anyway and report `crc_ok = false` (FFmpeg 9.0.2's default behaviour).
    #[default]
    Report,
    /// Refuse the access unit with [`DecodeError::CrcMismatch`].
    Reject,
}

/// Decoder configuration.
#[derive(Clone, Copy, Debug)]
pub struct DecoderOptions {
    pub crc: CrcPolicy,
    pub noise: NoiseMode,
    pub compat: Compat,
}

/// Output conventions the decoder reproduces where A/52 leaves a choice or the
/// pinned reference decoder deviates from it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compat {
    /// The pinned reference decoder (FFmpeg 9.0.2): its seed-zero dither noise
    /// stream, its E-AC-3 HEBAP-4 VQ table offset and its first-frame conventions;
    /// E-AC-3 reduced sample rates are refused as it refuses them. Output is compared
    /// with the reference decoder, so this is the default.
    #[default]
    Reference,
    /// A/52:2018 only; also decodes E-AC-3 reduced sample rates.
    Specification,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        DecoderOptions {
            crc: CrcPolicy::default(),
            noise: NoiseMode::default(),
            compat: Compat::Reference,
        }
    }
}

/// Summary of one decoded access unit.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    pub syntax: Syntax,
    pub bsid: u8,
    pub acmod: u8,
    pub lfeon: bool,
    pub dialnorm: u8,
    pub sample_rate: u32,
    /// Samples per channel appended to the output frame.
    pub samples: usize,
    pub layout: ChannelLayout,
    /// A 1+1 (dual mono) program: plane 0 is Ch1, plane 1 is Ch2.
    pub dual_mono: bool,
    /// Number of syncframes decoded (independent substream 0 plus its dependents).
    pub substreams: usize,
    /// False if any decoded syncframe failed its CRC check.
    pub crc_ok: bool,
    /// Set when some or all blocks were replaced by a repeat of the last decoded block
    /// (A/52 7.10 "block repeat" concealment, which FFmpeg 9.0.2 also applies).
    pub concealed: Option<DecodeError>,
    pub features: FrameFeatures,
    /// Downmix metadata codes of independent substream 0 as transmitted.
    pub downmix_codes: DownmixCodes,
    /// Downmix levels with the semantics of FFmpeg's `AV_FRAME_DATA_DOWNMIX_INFO`
    /// (exported only for programs with more than two channels).
    pub downmix: Option<DownmixInfo>,
}

/// Preferred stereo downmix (dmixmod, Annex D Table D2.2; '11' = Pro Logic II as FFmpeg
/// reports it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownmixType {
    Unknown,
    LoRo,
    LtRt,
    DplII,
}

/// Downmix levels as linear gains, mirroring FFmpeg 9.0.2's `AVDownmixInfo` export.
/// Every field is `Some` when the info is exported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownmixInfo {
    pub preferred_downmix_type: Option<DownmixType>,
    /// Lo/Ro center mix level.
    pub center_mix_level: Option<f64>,
    pub center_mix_level_ltrt: Option<f64>,
    /// Lo/Ro surround mix level.
    pub surround_mix_level: Option<f64>,
    pub surround_mix_level_ltrt: Option<f64>,
    pub lfe_mix_level: Option<f64>,
}

impl DownmixInfo {
    /// The stereo downmix FFmpeg's resampler applies: the Lt/Rt levels and surround
    /// encoding when the stream prefers an Lt/Rt (or Pro Logic II) downmix, else the
    /// Lo/Ro levels; absent levels keep the defaults.
    pub fn levels(&self) -> DownmixLevels {
        let default = DownmixLevels::default();
        let (center, surround, encoding) = match self.preferred_downmix_type {
            Some(DownmixType::LtRt) => (
                self.center_mix_level_ltrt,
                self.surround_mix_level_ltrt,
                SurroundEncoding::Dolby,
            ),
            Some(DownmixType::DplII) => (
                self.center_mix_level_ltrt,
                self.surround_mix_level_ltrt,
                SurroundEncoding::ProLogicII,
            ),
            _ => (
                self.center_mix_level,
                self.surround_mix_level,
                SurroundEncoding::None,
            ),
        };
        DownmixLevels {
            center: center.map_or(default.center, |v| v as f32),
            surround: surround.map_or(default.surround, |v| v as f32),
            lfe: self.lfe_mix_level.map_or(default.lfe, |v| v as f32),
            encoding,
        }
    }
}

/// Annex D Tables D2.3-D2.6 gains for the 3-bit mix level codes (+3 dB .. -6 dB, -inf),
/// i.e. powers of 2^(1/4); FFmpeg 9.0.2 also uses this table for the reserved
/// surround codes 0..2.
const MIX_GAINS: [f64; 8] = [
    core::f64::consts::SQRT_2,
    1.189_207_115_002_721,
    1.0,
    0.840_896_415_253_714_5,
    core::f64::consts::FRAC_1_SQRT_2,
    0.594_603_557_501_360_5,
    0.5,
    0.0,
];

/// Derive FFmpeg-compatible downmix info from one syncframe's header fields
/// (black-box verified).
fn downmix_info(eac3: bool, acmod: u8, c: &DownmixCodes) -> Option<DownmixInfo> {
    if acmod <= 2 {
        return None;
    }
    let g = |code: u8| MIX_GAINS[(code & 7) as usize];
    if !eac3 {
        // Tables 5.9 / 5.10 (reserved codes as the intermediate -4.5 / -6 dB values);
        // absent fields default to -4.5 dB / -6 dB. Lt/Rt levels are not carried by the
        // main-body syntax (FFmpeg ignores the Annex D xbsi1 fields) and report -3 dB.
        let clev = match c.cmixlev {
            Some(0) => g(4),
            Some(2) => g(6),
            _ => g(5),
        };
        let slev = match c.surmixlev {
            Some(0) => g(4),
            Some(2) => g(7),
            _ => g(6),
        };
        return Some(DownmixInfo {
            preferred_downmix_type: Some(DownmixType::Unknown),
            center_mix_level: Some(clev),
            center_mix_level_ltrt: Some(g(4)),
            surround_mix_level: Some(slev),
            surround_mix_level_ltrt: Some(g(4)),
            lfe_mix_level: Some(0.0),
        });
    }
    let pref = match c.dmixmod {
        Some(1) => DownmixType::LtRt,
        Some(2) => DownmixType::LoRo,
        Some(3) => DownmixType::DplII,
        _ => DownmixType::Unknown,
    };
    Some(DownmixInfo {
        preferred_downmix_type: Some(pref),
        center_mix_level: Some(g(c.lorocmixlev.unwrap_or(5))),
        center_mix_level_ltrt: Some(g(c.ltrtcmixlev.unwrap_or(0))),
        surround_mix_level: Some(g(c.lorosurmixlev.unwrap_or(6))),
        surround_mix_level_ltrt: Some(g(c.ltrtsurmixlev.unwrap_or(0))),
        // Annex E 2.3.1.11: LFE mix level (dB) = 10 - lfemixlevcod; absent = disabled.
        lfe_mix_level: Some(match c.lfemixlevcod {
            Some(code) => 10f64.powf((10.0 - code as f64) / 20.0),
            None => 0.0,
        }),
    })
}

struct SyncframeRef {
    offset: usize,
    header: FrameHeader,
}

/// AC-3 / E-AC-3 decoder.
#[derive(Debug)]
pub struct Decoder {
    opts: DecoderOptions,
    buf: Box<[u8]>,
    imdct: Box<imdct::Imdct>,
    groups: Box<GroupTables>,
    rng: noise::Rng,
    indep: Box<Substream>,
    deps: Vec<Option<Box<Substream>>>,
    last_block: Box<[[f32; 256]; MAX_CHANNELS]>,
    /// Layout and dual-mono flag of the last decoded block, for concealment.
    last_layout: Option<(ChannelLayout, bool)>,
    scratch: Box<[f32; 256]>,
    trace: Trace,
    quirk_armed: bool,
    last_downmix: Option<DownmixInfo>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::with_options(DecoderOptions::default())
    }

    pub fn with_options(opts: DecoderOptions) -> Self {
        let mut deps = Vec::with_capacity(8);
        deps.resize_with(8, || None);
        Decoder {
            opts,
            buf: vec![0u8; MAX_FRAME_BYTES + PAD].into_boxed_slice(),
            imdct: imdct::Imdct::new(),
            groups: Box::new(GroupTables::new()),
            rng: noise::Rng::new(
                opts.compat == Compat::Reference && opts.noise == NoiseMode::Normal,
            ),
            indep: Substream::new(),
            deps,
            last_block: Box::new([[0.0; 256]; MAX_CHANNELS]),
            last_layout: None,
            scratch: Box::new([0.0; 256]),
            trace: Trace::default(),
            quirk_armed: opts.compat == Compat::Reference,
            last_downmix: None,
        }
    }

    /// Drop overlap/delay history and restart the compatible noise stream.
    pub fn reset(&mut self) {
        self.rng.reset_compatible();
        self.indep.reset();
        for d in self.deps.iter_mut().flatten() {
            d.reset();
        }
        for b in &mut self.last_block {
            b.fill(0.0);
        }
        self.last_layout = None;
        self.quirk_armed = self.opts.compat == Compat::Reference;
    }

    #[doc(hidden)]
    pub fn set_trace(&mut self, on: bool) {
        self.trace.enabled = on;
    }

    #[doc(hidden)]
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    /// Decode one access unit: one AC-3 syncframe, or one E-AC-3 independent substream 0
    /// syncframe plus its dependent substreams (other independent programs are skipped).
    /// Appends the decoded samples to `out` (which must be empty or hold the same layout).
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<FrameInfo, DecodeError> {
        let first = parse_header(au)?;
        if first.stream_type == StreamType::Dependent || first.substream_id != 0 {
            return Err(DecodeError::MissingIndependentSubstream);
        }
        // Enumerate syncframes of program 0.
        let mut frames: [Option<SyncframeRef>; 9] = Default::default();
        let mut nframes = 0usize;
        let mut off = 0usize;
        let mut in_program0 = true;
        let mut seen = 0;
        // Sample rate reported for the access unit (see the non-compatible dependent below).
        let mut out_rate = first.sample_rate;
        while off < au.len() && seen < MAX_FRAMES_PER_AU {
            seen += 1;
            let h = match parse_header(&au[off..]) {
                Ok(h) => h,
                Err(e) if off == 0 => return Err(e),
                Err(_) => break,
            };
            if off > 0 {
                if h.syntax == Syntax::Ac3 {
                    break;
                }
                if h.stream_type != StreamType::Dependent {
                    in_program0 = false;
                }
            }
            if in_program0 && nframes < 9 {
                if off > 0 && h.syntax != Syntax::Eac3 {
                    break;
                }
                if off > 0
                    && (h.num_blocks != first.num_blocks || h.sample_rate != first.sample_rate)
                {
                    if self.opts.compat != Compat::Reference {
                        return Err(DecodeError::InvalidData(
                            "dependent substream block count/rate mismatch",
                        ));
                    }
                    // FFmpeg 9.0.2 drops a dependent whose rate or block count differs from the
                    // independent substream ("non-compatible") and outputs the core alone, e.g.
                    // a bsid 9 core (24 kHz) with a 48 kHz dependent. Behind a bsid 9/10 core
                    // it then labels the output with the dependent's rate (an FFmpeg quirk,
                    // black-box); behind any other core the core's rate is kept.
                    if first.syntax == Syntax::Ac3 && first.bsid > 8 {
                        out_rate = h.sample_rate;
                    }
                    off += h.frame_bytes;
                    continue;
                }
                frames[nframes] = Some(SyncframeRef {
                    offset: off,
                    header: h,
                });
                nframes += 1;
            }
            off += h.frame_bytes;
        }
        // FFmpeg 9.0.2 refuses E-AC-3 reduced sample rates (fscod2) in any substream it would
        // decode and outputs nothing for the access unit (black-box).
        if self.opts.compat == Compat::Reference
            && frames[..nframes]
                .iter()
                .flatten()
                .any(|f| f.header.syntax == Syntax::Eac3 && f.header.fscod == 3)
        {
            return Err(DecodeError::Unsupported(Feature::Eac3ReducedSampleRate));
        }
        let first_ref = frames[0].as_ref().expect("first frame");
        let truncated_first = first_ref.header.frame_bytes > au.len();

        // Channel layout of the program.
        let mut speakers = [Speaker::FrontCenter; MAX_CHANNELS];
        let mut nsp = 0usize;
        let mut maps: [PlaneMap; 9] = [[None; NCH]; 9];
        let mut coded_speakers: [[Option<Speaker>; NCH]; 9] = [[None; NCH]; 9];
        for (i, f) in frames.iter().enumerate().take(nframes) {
            let f = f.as_ref().unwrap();
            let h = &f.header;
            if i > 0 && f.offset + h.frame_bytes > au.len() {
                // A truncated dependent substream cannot be decoded.
                return Err(DecodeError::Truncated {
                    needed: f.offset + h.frame_bytes,
                    available: au.len(),
                });
            }
            let nf = substream::NFCHANS[h.acmod as usize];
            let chanmap = if h.stream_type == StreamType::Dependent {
                peek_chanmap(&au[f.offset..f.offset + h.frame_bytes])
            } else {
                None
            };
            let cs = &mut coded_speakers[i];
            match chanmap {
                Some(map) => {
                    let mut list = [Speaker::FrontCenter; 20];
                    let mut n = 0;
                    let mut has_lfe = false;
                    for bit in 0..16 {
                        if map & (0x8000 >> bit) == 0 {
                            continue;
                        }
                        let s: &[Speaker] = match bit {
                            0 => &[Speaker::FrontLeft],
                            1 => &[Speaker::FrontCenter],
                            2 => &[Speaker::FrontRight],
                            3 => &[Speaker::SideLeft],
                            4 => &[Speaker::SideRight],
                            5 => &[Speaker::FrontLeftOfCenter, Speaker::FrontRightOfCenter],
                            6 => &[Speaker::BackLeft, Speaker::BackRight],
                            7 => &[Speaker::BackCenter],
                            8 => &[Speaker::TopCenter],
                            9 => &[Speaker::SurroundDirectLeft, Speaker::SurroundDirectRight],
                            10 => &[Speaker::WideLeft, Speaker::WideRight],
                            11 => &[Speaker::TopFrontLeft, Speaker::TopFrontRight],
                            12 => &[Speaker::TopFrontCenter],
                            13 => &[Speaker::TopSideLeft, Speaker::TopSideRight],
                            14 => &[Speaker::LowFrequency2],
                            _ => {
                                has_lfe = true;
                                &[]
                            }
                        };
                        for &sp in s {
                            if n < list.len() {
                                list[n] = sp;
                                n += 1;
                            }
                        }
                    }
                    if n != nf || has_lfe != h.lfeon {
                        return Err(DecodeError::InvalidData(
                            "chanmap does not match acmod/lfeon",
                        ));
                    }
                    for c in 0..nf {
                        cs[c] = Some(list[c]);
                    }
                    if h.lfeon {
                        cs[LFE] = Some(Speaker::LowFrequency);
                    }
                }
                None => {
                    for (c, &sp) in acmod_channels(h.acmod).iter().enumerate() {
                        cs[c] = Some(sp);
                    }
                    if h.lfeon {
                        cs[LFE] = Some(Speaker::LowFrequency);
                    }
                }
            }
            for sp in cs.iter().flatten() {
                if !speakers[..nsp].contains(sp) {
                    if nsp == MAX_CHANNELS {
                        return Err(DecodeError::InvalidData("more than 16 channels"));
                    }
                    speakers[nsp] = *sp;
                    nsp += 1;
                }
            }
        }
        // Native order; the speakers are unique by construction.
        speakers[..nsp].sort_unstable();
        let layout = ChannelLayout::new(speakers[..nsp].iter().copied()).expect("16 speakers");
        let dual_mono = first.acmod == 0;
        // Planes: a speaker written by a later substream replaces the earlier one.
        for i in 0..nframes {
            for c in 0..NCH {
                if let Some(sp) = coded_speakers[i][c] {
                    maps[i][c] = layout.position(sp);
                }
            }
        }
        for i in 0..nframes {
            for c in 0..NCH {
                if let Some(p) = maps[i][c]
                    && (i + 1..nframes).any(|j| maps[j].contains(&Some(p)))
                {
                    maps[i][c] = None;
                }
            }
        }

        // Output frame bookkeeping.
        if out.samples_per_channel > 0 && (out.layout != layout || out.sample_rate != out_rate) {
            return Err(DecodeError::OutputLayoutMismatch);
        }
        let samples = first.samples();
        let base = out.samples_per_channel;

        if truncated_first {
            if first.syntax == Syntax::Eac3 {
                return Err(DecodeError::Truncated {
                    needed: first.frame_bytes,
                    available: au.len(),
                });
            }
            // AC-3: repeat the last decoded block (FFmpeg-compatible concealment).
            prepare_output(out, &layout, out_rate, base + samples);
            let same = self.last_layout == Some((layout, dual_mono));
            for p in 0..layout.channels() {
                for blk in 0..first.num_blocks {
                    let dst = &mut out.planes[p][base + blk * 256..base + blk * 256 + 256];
                    if same {
                        dst.copy_from_slice(&self.last_block[p]);
                    } else {
                        dst.fill(0.0);
                    }
                }
            }
            return Ok(FrameInfo {
                syntax: first.syntax,
                bsid: first.bsid,
                acmod: first.acmod,
                lfeon: first.lfeon,
                dialnorm: 0,
                sample_rate: out_rate,
                samples,
                layout,
                dual_mono,
                substreams: 0,
                crc_ok: false,
                concealed: Some(DecodeError::Truncated {
                    needed: first.frame_bytes,
                    available: au.len(),
                }),
                features: FrameFeatures::default(),
                downmix_codes: self.indep.downmix,
                // The header of the truncated frame is not decoded; repeat the last info.
                downmix: if same {
                    self.last_downmix
                } else {
                    downmix_info(false, first.acmod, &DownmixCodes::default())
                },
            });
        }

        // CRC checks.
        let mut crc_ok = true;
        for f in frames.iter().take(nframes).flatten() {
            let fb = &au[f.offset..f.offset + f.header.frame_bytes];
            let (c1, c2) = check_crc(fb, &f.header);
            if !(c1 && c2) {
                crc_ok = false;
                if self.opts.crc == CrcPolicy::Reject {
                    return Err(DecodeError::CrcMismatch {
                        crc1_ok: c1,
                        crc2_ok: c2,
                    });
                }
            }
        }

        prepare_output(out, &layout, out_rate, base + samples);
        self.trace.frames.clear();
        let mut concealed = None;
        let mut info_dialnorm = 0;
        let mut features = FrameFeatures::default();
        let mut downmix_codes = DownmixCodes::default();
        let mut downmix = None;
        let mut decoded = 0;
        for i in 0..nframes {
            let f = frames[i].as_ref().unwrap();
            let h = f.header;
            let fb = &au[f.offset..f.offset + h.frame_bytes];
            self.buf[..fb.len()].copy_from_slice(fb);
            self.buf[fb.len()..fb.len() + PAD].fill(0);
            let sub: &mut Substream = if i == 0 {
                &mut self.indep
            } else {
                let id = h.substream_id as usize;
                self.deps[id].get_or_insert_with(Substream::new)
            };
            sub.reference_compat = self.opts.compat == Compat::Reference;
            let mut sink = BlockSink {
                planes: &mut out.planes,
                base,
                map: maps[i],
                scratch: &mut self.scratch,
            };
            let ntrace = self.trace.frames.len();
            let r = sub.decode_frame(
                &self.buf[..fb.len() + PAD],
                h.frame_bytes,
                &mut self.imdct,
                &mut self.rng,
                self.opts.noise,
                &self.groups,
                &mut sink,
                &mut self.trace,
                &mut self.quirk_armed,
            );
            if let Some(t) = self.trace.frames.get_mut(ntrace) {
                t.offset = f.offset;
            }
            // FFmpeg exports the downmix info of the last syncframe it parsed in the AU.
            downmix = downmix_info(sub.eac3, sub.acmod, &sub.downmix);
            if i == 0 {
                info_dialnorm = sub.dialnorm;
                features = sub.features;
                downmix_codes = sub.downmix;
            }
            decoded += 1;
            if let Err((blk, e)) = r {
                if blk == 0
                    && i == 0
                    && matches!(e, DecodeError::Unsupported(Feature::ReservedSampleRate))
                {
                    return Err(e);
                }
                concealed.get_or_insert(e);
                // Repeat the last good block for the failed and following blocks.
                for c in 0..NCH {
                    let Some(p) = maps[i][c] else { continue };
                    for b in blk..h.num_blocks {
                        let dst = base + b * 256;
                        if b == 0 {
                            let src = if self.last_layout == Some((layout, dual_mono)) {
                                self.last_block[p]
                            } else {
                                [0.0; 256]
                            };
                            out.planes[p][dst..dst + 256].copy_from_slice(&src);
                        } else {
                            out.planes[p].copy_within(dst - 256..dst, dst);
                        }
                    }
                }
            }
        }
        // Remember the last block for concealment.
        let last = base + samples - 256;
        for p in 0..layout.channels() {
            self.last_block[p].copy_from_slice(&out.planes[p][last..last + 256]);
        }
        self.last_layout = Some((layout, dual_mono));
        self.last_downmix = downmix;
        Ok(FrameInfo {
            syntax: first.syntax,
            bsid: first.bsid,
            acmod: first.acmod,
            lfeon: first.lfeon,
            dialnorm: info_dialnorm,
            sample_rate: out_rate,
            samples,
            layout,
            dual_mono,
            substreams: decoded,
            crc_ok,
            concealed,
            features,
            downmix_codes,
            downmix,
        })
    }
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        out.clear();
        let info = Decoder::decode(self, au, out).map_err(DecodeFailure::from_error)?;
        Ok(info.downmix.map(|info| info.levels()))
    }
    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

fn prepare_output(out: &mut AudioFrame, layout: &ChannelLayout, rate: u32, len: usize) {
    out.layout = *layout;
    out.sample_rate = rate;
    out.planes.resize_with(layout.channels(), Vec::new);
    for p in &mut out.planes {
        p.resize(len, 0.0);
    }
    out.samples_per_channel = len;
}

/// Returns (crc1 ok, crc2 ok). E-AC-3 has a single CRC; it is reported in both.
fn check_crc(frame: &[u8], h: &FrameHeader) -> (bool, bool) {
    let words = frame.len() / 2;
    match h.syntax {
        Syntax::Ac3 => {
            let five8 = (words >> 1) + (words >> 3);
            let c1 = crc16(0, &frame[2..five8 * 2]) == 0;
            let c2 = crc16(0, &frame[2..]) == 0;
            (c1, c2)
        }
        Syntax::Eac3 => {
            let c = crc16(0, &frame[2..]) == 0;
            (c, c)
        }
    }
}

/// Read the custom channel map of an E-AC-3 dependent substream (Annex E Table E1.2).
fn peek_chanmap(frame: &[u8]) -> Option<u16> {
    let mut b = [0u8; 24];
    let n = frame.len().min(16);
    b[..n].copy_from_slice(&frame[..n]);
    let mut r = BitReader::at(&b, 16);
    let strmtyp = r.read(2);
    r.skip(3 + 11);
    let fscod = r.read(2);
    r.skip(2); // fscod2 or numblkscod
    let _ = fscod;
    let acmod = r.read(3);
    r.skip(1 + 5 + 5); // lfeon, bsid, dialnorm
    if r.bit() {
        r.skip(8);
    }
    if acmod == 0 {
        r.skip(5);
        if r.bit() {
            r.skip(8);
        }
    }
    if strmtyp == 1 && r.bit() {
        Some(r.read(16) as u16)
    } else {
        None
    }
}

#[cfg(test)]
mod size_tests {
    #[test]
    fn decoder_state_is_bounded() {
        let sub = size_of::<crate::substream::Substream>();
        let aht = size_of::<[[[f32; 256]; 6]; crate::substream::NCH]>();
        eprintln!(
            "Substream {sub} bytes (+{aht} AHT heap), Decoder {} bytes",
            size_of::<crate::Decoder>()
        );
        assert!(sub + aht < 128 * 1024);
    }
}
