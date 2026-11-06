//! Decoding of one syncframe of one substream: bsi, (E-AC-3) audfrm, audio blocks,
//! exponents, bit allocation, mantissas, coupling, rematrixing, spectral extension, AHT,
//! dynamic range and the inverse transform.

mod bit_allocation;
mod block;
mod bsi;
mod coupling;
#[cfg(feature = "debug-dump")]
mod debug_dump;
mod exponents;
mod mantissas;
mod reconstruct;

use crate::bitalloc::{BaCache, BaParams, Dba};
use crate::bits::BitReader;
use crate::error::DecodeError;
use crate::imdct::Imdct;
use crate::noise::Rng;
use crate::quant::GroupTables;

pub(crate) const CPL: usize = 5;
pub(crate) const LFE: usize = 6;
pub(crate) const NCH: usize = 7;

const REUSE: u8 = 0;

type E = DecodeError;

#[inline]
fn bad(what: &'static str) -> E {
    DecodeError::InvalidData(what)
}

/// Noise behaviour (diagnostic switch; normal decoding uses `Normal`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NoiseMode {
    /// Dither and SPX noise as the specification describes.
    #[default]
    Normal,
    /// Replace dither, SPX noise and AHT zero-bin noise by zero.
    Off,
}

/// Bit positions recorded for test tooling (dither stripping).
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub enabled: bool,
    /// One record per decoded syncframe of the last access unit.
    pub frames: Vec<FrameTrace>,
}

#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct FrameTrace {
    /// Byte offset of the syncframe within the access unit.
    pub offset: usize,
    pub frame_bytes: usize,
    pub eac3: bool,
    pub num_blocks: usize,
    pub nfchans: usize,
    /// Bit positions (within the syncframe) of every dithflag bit.
    pub dithflag_bits: Vec<usize>,
    /// E-AC-3: bit position of dithflage, and where per-block dithflags would be inserted.
    pub dithflage_bit: Option<usize>,
    pub dithflag_insert_bits: Vec<usize>,
    /// Bit position just after the last mantissa of the last block.
    pub audio_end_bit: usize,
    /// E-AC-3: bit positions of every ecplinu flag (blocks with a coupling strategy).
    pub ecplinu_bits: Vec<usize>,
    /// E-AC-3: bit position of transproce and of the (possibly absent) TPNP data.
    pub transproce_bit: Option<usize>,
    pub transproc_data_bit: Option<usize>,
    pub dbaflde_bit: Option<usize>,
    pub blocks: Vec<BlockTrace>,
    pub features: FrameFeatures,
}

#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct BlockTrace {
    pub blk: usize,
    pub cplstre: bool,
    pub phsflginu_bit: Option<usize>,
    pub phsflg_bit: usize,
    pub ncplbnd: usize,
    pub cplcoe_any: bool,
    pub dba_bit: usize,
}

impl Trace {
    fn cur(&mut self) -> Option<&mut FrameTrace> {
        if self.enabled {
            self.frames.last_mut()
        } else {
            None
        }
    }
}

/// Downmix metadata as transmitted (raw codes; `None` when the field is absent).
/// AC-3: cmixlev/surmixlev (Tables 5.9/5.10), and for bsid 6 the Annex D xbsi1 fields.
/// E-AC-3: the mixmdate fields of Annex E Table E1.2 (code tables of Annex D).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownmixCodes {
    pub cmixlev: Option<u8>,
    pub surmixlev: Option<u8>,
    pub dmixmod: Option<u8>,
    pub ltrtcmixlev: Option<u8>,
    pub ltrtsurmixlev: Option<u8>,
    pub lorocmixlev: Option<u8>,
    pub lorosurmixlev: Option<u8>,
    pub lfemixlevcod: Option<u8>,
}

/// Coding tools used by a syncframe (diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameFeatures {
    pub coupling: bool,
    pub phase_flags: bool,
    pub rematrix: bool,
    pub block_switch: bool,
    pub dither: bool,
    pub delta_ba: bool,
    pub dynrng: bool,
    pub spx: bool,
    pub spx_atten: bool,
    pub aht: bool,
    pub gaq: bool,
    pub tpnp: bool,
    pub frame_expstr: bool,
    pub skip_fields: bool,
    pub ecpl: bool,
    /// Some audio block read past the end of the syncframe (corrupt or patched data).
    pub overrun: bool,
}

#[derive(Debug)]
pub(crate) struct Substream {
    // ---- frame configuration ----
    pub eac3: bool,
    pub strmtyp: u8,
    pub acmod: u8,
    pub lfeon: bool,
    pub nfchans: usize,
    pub fscod: u8,
    /// Index into the hearing-threshold table (fscod, or fscod2 for reduced rates).
    hth_idx: usize,
    /// AC-3 bsid 9/10 (half/quarter rate) sample-rate shift for the bit allocation
    /// (decays and hth band; FFmpeg 9.0.2, black-box; A/52:2018 has no such rule). 0 otherwise.
    rate_shift: u32,
    pub num_blocks: usize,
    pub bsid: u8,
    pub dialnorm: u8,
    pub chanmap: Option<u16>,
    pub downmix: DownmixCodes,
    config_key: u32,

    // ---- E-AC-3 audfrm ----
    ahte: bool,
    snroffststr: u8,
    blkswe: bool,
    dithflage: bool,
    bamode: bool,
    frmfgaincode: bool,
    dbaflde: bool,
    skipflde: bool,
    cplstre_blk: [bool; 6],
    cplinu_blk: [bool; 6],
    /// Exponent strategy per channel (fbw, CPL, LFE) per block.
    expstr: [[u8; 6]; NCH],
    aht: [bool; NCH],
    aht_read: [bool; NCH],
    frmcsnroffst: u8,
    frmfsnroffst: u8,
    chinspxatten: [bool; 5],
    spxattencod: [u8; 5],
    firstspxcos: [bool; 5],
    firstcplcos: [bool; 5],
    firstcplleak: bool,
    cplbndstrc_set_in_frame: bool,

    // ---- block state ----
    blksw: [bool; 5],
    dithflag: [bool; 5],
    dynrng: [f32; 2],

    spxinu: bool,
    chinspx: [bool; 5],
    spxstrtf: usize,
    spxbegf: usize,
    spx_begin: usize,
    spx_end: usize,
    spxbndstrc: [bool; 18],
    nspxbnds: usize,
    spxbndsz: [usize; 18],
    spxco: [[f32; 18]; 5],
    nblend: [[f32; 18]; 5],
    sblend: [[f32; 18]; 5],

    cplinu: bool,
    chincpl: [bool; 5],
    phsflginu: bool,
    phsflginu_pos: Option<usize>,
    cplbegf: i32,
    cplendf: i32,
    cplbndstrc: [bool; 18],
    ncplsubnd: usize,
    ncplbnd: usize,
    /// Coupling band index of each coupling sub-band (relative to cplbegf).
    cpl_sb_band: [u8; 18],
    cplco: [[f32; 18]; 5],
    phsflg: [bool; 18],
    cplstrtmant: usize,
    cplendmant: usize,

    rematflg: [bool; 4],
    nrematbd: usize,

    exps: [[u8; 256]; NCH],
    endmant: [usize; NCH],
    strtmant: [usize; NCH],
    bap: [[u8; 256]; NCH],
    /// Bit allocation stages to re-run per channel: 0 none, 1 bap, 2 mask+bap, 3 all.
    ba_dirty: [u8; NCH],
    ba_cache: [BaCache; NCH],
    ba: BaParams,
    csnroffst: u8,
    fsnroffst: [u8; NCH],
    fgaincod: [u8; NCH],
    cplfleak: u8,
    cplsleak: u8,
    dba: [Dba; NCH],

    /// AHT: dequantized and inverse-DCT'd mantissas [ch][blk][bin] (before exponents).
    aht_coef: Box<[[[f32; 256]; 6]; NCH]>,
    /// Current block transform coefficients as Q23 integers after the exponent shift.
    icoef: [[i32; 256]; NCH],
    /// Current block transform coefficients (float, dynamic range applied).
    coef: [[f32; 256]; NCH],
    /// Overlap-add delay lines per coded channel (index CPL unused).
    pub delay: [[f32; 256]; NCH],

    pub features: FrameFeatures,
    pub reference_compat: bool,
}

/// Output routing for one substream: coded channel -> plane index.
pub(crate) type PlaneMap = [Option<usize>; NCH];

pub(crate) struct BlockSink<'a> {
    pub planes: &'a mut [Vec<f32>],
    /// Sample offset of block 0 within each plane.
    pub base: usize,
    pub map: PlaneMap,
    pub scratch: &'a mut [f32; 256],
}

/// Speaker/channel description of a coded channel, used by the layout logic.
pub(crate) fn acmod_channels(acmod: u8) -> &'static [crate::Speaker] {
    use crate::Speaker::{BackCenter, FrontCenter, FrontLeft, FrontRight, SideLeft, SideRight};
    match acmod {
        0 => &[FrontLeft, FrontRight],
        1 => &[FrontCenter],
        2 => &[FrontLeft, FrontRight],
        3 => &[FrontLeft, FrontCenter, FrontRight],
        4 => &[FrontLeft, FrontRight, BackCenter],
        5 => &[FrontLeft, FrontCenter, FrontRight, BackCenter],
        6 => &[FrontLeft, FrontRight, SideLeft, SideRight],
        _ => &[FrontLeft, FrontCenter, FrontRight, SideLeft, SideRight],
    }
}

pub(crate) const NFCHANS: [usize; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

impl Substream {
    pub(crate) fn new() -> Box<Self> {
        Box::new(Substream {
            eac3: false,
            strmtyp: 0,
            acmod: 0,
            lfeon: false,
            nfchans: 0,
            fscod: 0,
            hth_idx: 0,
            rate_shift: 0,
            num_blocks: 6,
            bsid: 0,
            dialnorm: 0,
            chanmap: None,
            downmix: DownmixCodes::default(),
            config_key: u32::MAX,
            ahte: false,
            snroffststr: 0,
            blkswe: false,
            dithflage: false,
            bamode: false,
            frmfgaincode: false,
            dbaflde: false,
            skipflde: false,
            cplstre_blk: [false; 6],
            cplinu_blk: [false; 6],
            expstr: [[0; 6]; NCH],
            aht: [false; NCH],
            aht_read: [false; NCH],
            frmcsnroffst: 0,
            frmfsnroffst: 0,
            chinspxatten: [false; 5],
            spxattencod: [0; 5],
            firstspxcos: [true; 5],
            firstcplcos: [true; 5],
            firstcplleak: true,
            cplbndstrc_set_in_frame: false,
            blksw: [false; 5],
            dithflag: [false; 5],
            dynrng: [1.0; 2],
            spxinu: false,
            chinspx: [false; 5],
            spxstrtf: 0,
            spxbegf: 0,
            spx_begin: 0,
            spx_end: 0,
            spxbndstrc: [false; 18],
            nspxbnds: 0,
            spxbndsz: [0; 18],
            spxco: [[0.0; 18]; 5],
            nblend: [[0.0; 18]; 5],
            sblend: [[0.0; 18]; 5],
            cplinu: false,
            chincpl: [false; 5],
            phsflginu: false,
            phsflginu_pos: None,
            cplbegf: 0,
            cplendf: 0,
            cplbndstrc: [false; 18],
            ncplsubnd: 0,
            ncplbnd: 0,
            cpl_sb_band: [0; 18],
            cplco: [[0.0; 18]; 5],
            phsflg: [false; 18],
            cplstrtmant: 0,
            cplendmant: 0,
            rematflg: [false; 4],
            nrematbd: 0,
            exps: [[0; 256]; NCH],
            endmant: [0; NCH],
            strtmant: [0; NCH],
            bap: [[0; 256]; NCH],
            ba_dirty: [3; NCH],
            ba_cache: Default::default(),
            ba: BaParams::default(),
            csnroffst: 0,
            fsnroffst: [0; NCH],
            fgaincod: [0; NCH],
            cplfleak: 0,
            cplsleak: 0,
            dba: [Dba::default(); NCH],
            #[allow(clippy::large_stack_arrays)] // Built once per decoder.
            aht_coef: Box::new([[[0.0; 256]; 6]; NCH]),
            icoef: [[0; 256]; NCH],
            coef: [[0.0; 256]; NCH],
            delay: [[0.0; 256]; NCH],
            features: FrameFeatures::default(),
            reference_compat: true,
        })
    }

    pub(crate) fn reset(&mut self) {
        for d in &mut self.delay {
            d.fill(0.0);
        }
        self.config_key = u32::MAX;
        self.ba_dirty = [3; NCH];
    }

    /// Coded channel indices of this substream in coded order (fbw..., LFE).
    pub(crate) fn coded_channels(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nfchans).chain(self.lfeon.then_some(LFE))
    }

    /// Decode one syncframe already copied (with zero padding) into `buf`.
    /// On error returns the number of blocks fully output before the error.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decode_frame(
        &mut self,
        buf: &[u8],
        frame_bytes: usize,
        imdct: &mut Imdct,
        rng: &mut Rng,
        noise: NoiseMode,
        groups: &GroupTables,
        sink: &mut BlockSink<'_>,
        trace: &mut Trace,
        quirk_armed: &mut bool,
    ) -> Result<(), (usize, E)> {
        let mut br = BitReader::new(buf, 16);
        let eac3 = buf[5] >> 3 > 10;
        self.features = FrameFeatures::default();
        let r = if eac3 {
            self.parse_bsi_eac3(&mut br)
        } else {
            self.parse_bsi_ac3(&mut br)
        };
        r.map_err(|e| (0, e))?;
        if trace.enabled {
            trace.frames.push(FrameTrace {
                frame_bytes,
                eac3,
                num_blocks: self.num_blocks,
                nfchans: self.nfchans,
                ..Default::default()
            });
        }
        if eac3 {
            self.parse_audfrm(&mut br, frame_bytes / 2, trace)
                .map_err(|e| (0, e))?;
        }
        // Delta bit allocation and dynamic range do not persist across syncframes.
        for (d, dirty) in self.dba.iter_mut().zip(self.ba_dirty.iter_mut()) {
            if d.active {
                d.active = false;
                *dirty = (*dirty).max(2);
            }
        }
        self.dynrng = [1.0; 2];
        let frame_bits = frame_bytes * 8;
        for blk in 0..self.num_blocks {
            self.parse_block(&mut br, blk, rng, noise, groups, trace)
                .map_err(|e| (blk, e))?;
            if br.pos() > frame_bits {
                // FFmpeg 9.0.2 keeps decoding with zero bits past the frame end; A/52 7.10
                // leaves error handling to the implementation.
                self.features.overrun = true;
                if !self.reference_compat {
                    return Err((blk, bad("audio block overruns syncframe")));
                }
            }
            self.reconstruct(rng, noise);
            #[cfg(feature = "debug-dump")]
            self.debug_dump_block();
            if *quirk_armed && (1..self.nfchans).any(|ch| self.blksw[ch] != self.blksw[0]) {
                *quirk_armed = false;
                self.ffmpeg_first_mixed_transform_quirk();
            }
            self.synthesize(blk, imdct, sink);
        }
        if let Some(t) = trace.cur() {
            t.audio_end_bit = br.pos();
            t.features = self.features;
        }
        Ok(())
    }
}

#[cfg(test)]
mod noise_tests {
    use super::*;
    use crate::tables::spx_band_start;

    #[test]
    fn coupled_bins_draw_at_the_first_channel_even_if_it_suppresses_noise() {
        let mut sub = Substream::new();
        sub.nfchans = 2;
        sub.cplinu = true;
        sub.cplstrtmant = 37;
        sub.cplendmant = 49;
        sub.ncplsubnd = 1;
        sub.chincpl[..2].fill(true);
        sub.endmant[..2].fill(37);
        sub.dithflag[1] = true;
        sub.cplco[0][0] = 0.125;
        sub.cplco[1][0] = 0.125;

        let mut rng = Rng::new(true);
        let mut reader = BitReader::new(&[0; crate::bits::PAD], 0);
        sub.unpack_mantissas(
            &mut reader,
            0,
            &mut rng,
            NoiseMode::Normal,
            &GroupTables::new(),
        )
        .unwrap();
        assert_eq!(reader.pos(), 0);
        assert_eq!(sub.icoef[CPL][37], 4762085);
        assert_eq!(sub.icoef[1][0], -4643861);
        sub.reconstruct(&mut rng, NoiseMode::Normal);
        assert_eq!(sub.icoef[0][37..49], [0; 12]);
        assert_eq!(sub.icoef[1][37..49], sub.icoef[CPL][37..49]);
        assert_eq!(rng.dither_q23(), -374938);
    }

    #[test]
    fn spx_draws_in_channel_then_bin_order_even_for_a_zero_blend() {
        let mut sub = Substream::new();
        sub.eac3 = true;
        sub.nfchans = 2;
        sub.spxinu = true;
        sub.chinspx[..2].fill(true);
        sub.spx_begin = 1;
        sub.nspxbnds = 1;
        sub.spxbndsz[0] = 12;
        sub.spxco[0][0] = 1.0 / 32.0;
        sub.spxco[1][0] = 1.0 / 32.0;
        sub.nblend[1][0] = 1.0;
        let copy_start = spx_band_start(0);
        let extension_start = spx_band_start(1);
        sub.icoef[0][copy_start..extension_start].fill(1 << 23);
        sub.icoef[1][copy_start..extension_start].fill(1 << 23);
        let mut rng = Rng::new(true);

        sub.reconstruct(&mut rng, NoiseMode::Normal);
        assert!(
            sub.coef[0][extension_start..extension_start + 12]
                .iter()
                .all(|&v| v == 0.0)
        );
        assert_eq!(sub.coef[1][extension_start].to_bits(), 0xbe5e3a86);
        assert_eq!(sub.coef[1][extension_start + 11].to_bits(), 0x3f403f7b);
        assert_eq!(rng.dither_q23(), 555625);
    }
}
