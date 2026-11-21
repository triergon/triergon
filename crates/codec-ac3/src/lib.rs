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
mod downmix;
mod error;
mod header;
mod imdct;
mod layout;
mod noise;
mod quant;
mod spec_tables;
mod substream;
mod tables;

pub use downmix::{DownmixInfo, DownmixType};
pub use error::{DecodeError, Feature};
pub use header::{FrameHeader, StreamType, Syntax, access_unit_len, parse_header};
pub use layout::{MAX_CHANNELS, layout_name};
#[doc(hidden)]
pub use substream::{BlockTrace, FrameTrace, Trace};
pub use substream::{DownmixCodes, FrameFeatures, NoiseMode};
pub use triergon_bitstream::crc::crc16_a52 as crc16;
pub use triergon_codec::{AudioFrame, ChannelLayout, Speaker};

use downmix::downmix_info;
use header::{check_crc, peek_chanmap};
use quant::GroupTables;
use substream::{BlockSink, LFE, NCH, PlaneMap, Substream, acmod_channels};
use triergon_codec::{AudioDecoder, Budget, Charged, DownmixLevels, Reservation};

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

/// Decoder configuration; the default reproduces the reference decoder.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecoderOptions {
    /// What to do with syncframes whose CRC does not check.
    pub crc: CrcPolicy,
    /// Whether dither, AHT zero-bin and spectral extension noise is generated.
    pub noise: NoiseMode,
    /// Which output conventions to follow where A/52 leaves a choice.
    pub compat: Compat,
}

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Live storage that depends on the stream: dependent substream state and the
    /// output frame the decoder fills. The default holds eight dependent substreams
    /// and a 16-channel frame many times over.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 4 * 1024 * 1024,
        }
    }
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

/// Summary of one decoded access unit.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Syntax of independent substream 0.
    pub syntax: Syntax,
    /// Bit stream identification of independent substream 0 (0..=16).
    pub bsid: u8,
    /// Audio coding mode of independent substream 0 (A/52 Table 5.8: 0 = 1+1 ..= 7 = 3/2).
    pub acmod: u8,
    /// Independent substream 0 carries an LFE channel.
    pub lfeon: bool,
    /// Dialogue normalization code of independent substream 0 (1..=31 = -1..=-31 dB,
    /// 0 reserved); reported, not applied.
    pub dialnorm: u8,
    /// Output sample rate in Hz.
    pub sample_rate: u32,
    /// Samples per channel appended to the output frame.
    pub samples: usize,
    /// Speakers of the output planes, in plane order.
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
    /// Coding tools independent substream 0 used.
    pub features: FrameFeatures,
    /// Downmix metadata codes of independent substream 0 as transmitted.
    pub downmix_codes: DownmixCodes,
    /// Downmix levels with the semantics of FFmpeg's `AV_FRAME_DATA_DOWNMIX_INFO`
    /// (exported only for programs with more than two channels).
    pub downmix: Option<DownmixInfo>,
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
    /// Dependent substreams, created and charged when a stream first names them.
    deps: Vec<Option<Dependent>>,
    /// Pool of the stream-dependent storage.
    budget: Budget,
    /// Charge of the output frame the last access unit sized.
    output_charge: Reservation,
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
    /// A decoder with the default options and a pool of the default limit.
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`. Fails only for a
    /// limit below a pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        let opts = DecoderOptions::default();
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
            indep: Box::new(Substream::new()),
            deps,
            budget: budget.clone(),
            output_charge: Reservation::empty(budget),
            last_block: Box::new([[0.0; 256]; MAX_CHANNELS]),
            last_layout: None,
            scratch: Box::new([0.0; 256]),
            trace: Trace::default(),
            quirk_armed: opts.compat == Compat::Reference,
            last_downmix: None,
        }
    }

    /// The decoder with behaviour options `opts`, from its initial state.
    #[must_use]
    pub fn with_options(mut self, opts: DecoderOptions) -> Self {
        self.opts = opts;
        self.rng =
            noise::Rng::new(opts.compat == Compat::Reference && opts.noise == NoiseMode::Normal);
        self.quirk_armed = opts.compat == Compat::Reference;
        self
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

    /// Size `out` for `len` samples per channel of `layout`, charging it first.
    fn prepare_output(
        &mut self,
        out: &mut AudioFrame,
        layout: &ChannelLayout,
        rate: u32,
        len: usize,
    ) -> Result<(), DecodeError> {
        out.try_resize(layout.channels(), len, &mut self.output_charge)?;
        out.layout = *layout;
        out.sample_rate = rate;
        out.samples_per_channel = len;
        Ok(())
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
            self.prepare_output(out, &layout, out_rate, base + samples)?;
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

        self.prepare_output(out, &layout, out_rate, base + samples)?;
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
                dependent(&mut self.deps, &self.budget, h.substream_id as usize)?
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

/// A dependent substream's state, charged to the decoder's pool.
type Dependent = Charged<Box<Substream>>;

/// Dependent substream `id`, created and charged the first time a stream names it.
fn dependent<'a>(
    deps: &'a mut [Option<Dependent>],
    budget: &Budget,
    id: usize,
) -> Result<&'a mut Substream, DecodeError> {
    Ok(match &mut deps[id] {
        Some(sub) => sub,
        slot => slot.insert(Charged::boxed(budget, Substream::new())?),
    })
}

impl AudioDecoder for Decoder {
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        out.clear();
        let info = Decoder::decode(self, au, out)?;
        Ok(info.downmix.map(|info| info.levels()))
    }
    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use triergon_codec::StorageError;

    #[test]
    fn a_refused_dependent_substream_leaves_its_slot_empty() {
        let budget = Budget::new(64 * 1024).unwrap();
        let mut deps: Vec<Option<Dependent>> = (0..8).map(|_| None).collect();
        assert!(matches!(
            dependent(&mut deps, &budget, 3),
            Err(DecodeError::Budget(StorageError::Limit { .. }))
        ));
        assert!(deps[3].is_none());
        let budget = Budget::new(1 << 20).unwrap();
        dependent(&mut deps, &budget, 3).unwrap();
        let used = budget.used();
        dependent(&mut deps, &budget, 3).unwrap();
        assert_eq!(
            budget.used(),
            used,
            "an existing substream is not charged again"
        );
    }

    #[test]
    fn the_default_limit_holds_eight_dependents_and_a_full_frame() {
        let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
        let mut deps: Vec<Option<Dependent>> = (0..8).map(|_| None).collect();
        for id in 0..8 {
            dependent(&mut deps, &budget, id).unwrap();
        }
        let mut charge = Reservation::empty(&budget);
        AudioFrame::default()
            .try_resize(MAX_CHANNELS, 1536, &mut charge)
            .unwrap();
        assert!(budget.used() * 2 < budget.limit(), "{}", budget.used());
    }
}

#[cfg(test)]
mod size_tests {
    #[test]
    fn decoder_state_is_bounded() {
        let sub = size_of::<crate::substream::Substream>();
        eprintln!(
            "Substream {sub} bytes, Decoder {} bytes",
            size_of::<crate::Decoder>()
        );
        assert!(sub < 128 * 1024);
    }
}
