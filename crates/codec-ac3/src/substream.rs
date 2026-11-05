//! Decoding of one syncframe of one substream: bsi, (E-AC-3) audfrm, audio blocks,
//! exponents, bit allocation, mantissas, coupling, rematrixing, spectral extension, AHT,
//! dynamic range and the inverse transform.

use crate::bitalloc::{self, BaCache, BaParams, Dba, MaskParams};
use crate::bits::BitReader;
use crate::error::{DecodeError, Feature};
use crate::imdct::Imdct;
use crate::noise::Rng;
use crate::quant::{EXP_SCALE, GroupTables};
use crate::spec_tables::{
    VQ_HEBAP1, VQ_HEBAP2, VQ_HEBAP3, VQ_HEBAP4, VQ_HEBAP5, VQ_HEBAP6, VQ_HEBAP7,
};
use crate::tables::{
    BAP_BITS, DEF_CPL_BNDSTRC, DEF_SPX_BNDSTRC, FASTGAIN, FRM_EXPSTR, GAQ_A1, GAQ_A2, GAQ_A4,
    GAQ_B2_NEG, GAQ_B2_POS, GAQ_B4_NEG, GAQ_B4_POS, HEBAP_BITS, REMAT_BANDS, frac16, spx_atten,
    spx_band_start,
};

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

/// Coding tools used by a syncframe (diagnostics / STATUS reporting).
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
    pub ffmpeg_compat: bool,
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
    use crate::Speaker::*;
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

/// Per-block mantissa group state (7.3.5): groups are shared across exponent sets.
struct Groups {
    g1: [i32; 3],
    n1: usize,
    g2: [i32; 3],
    n2: usize,
    g4: [i32; 2],
    n4: usize,
}

impl Groups {
    fn new() -> Self {
        Groups {
            g1: [0; 3],
            n1: 0,
            g2: [0; 3],
            n2: 0,
            g4: [0; 2],
            n4: 0,
        }
    }
}

#[inline]
fn dynrng_gain(v: u32) -> f32 {
    // 7.7.1.2: X0X1X2.Y3..Y7, gain = 2^(X+1) * 0.1Y (base 2), X signed 3-bit.
    let x = ((v as u8 as i8) >> 5) as i32;
    let y = (v & 31) as f32;
    let m = (32.0 + y) / 64.0;
    if x + 1 >= 0 {
        m * (1u32 << (x + 1)) as f32
    } else {
        m / (1u32 << (-(x + 1))) as f32
    }
}

/// Resolve the exponent group count (7.1.3) for D15/D25/D45.
#[inline]
fn grp_size(strategy: u8) -> usize {
    match strategy {
        1 => 1,
        2 => 2,
        _ => 4,
    }
}

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
            aht_coef: Box::new([[[0.0; 256]; 6]; NCH]),
            icoef: [[0; 256]; NCH],
            coef: [[0.0; 256]; NCH],
            delay: [[0.0; 256]; NCH],
            features: FrameFeatures::default(),
            ffmpeg_compat: true,
        })
    }

    pub(crate) fn reset(&mut self) {
        for d in self.delay.iter_mut() {
            d.fill(0.0);
        }
        self.config_key = u32::MAX;
        self.ba_dirty = [3; NCH];
    }

    /// Coded channel indices of this substream in coded order (fbw..., LFE).
    pub(crate) fn coded_channels(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.nfchans).chain(self.lfeon.then_some(LFE))
    }

    // ------------------------------------------------------------------ bsi

    fn parse_bsi_ac3(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let _crc1 = br.get(16);
        let fscod = br.get(2) as u8;
        let _frmsizecod = br.get(6);
        let bsid = br.get(5) as u8;
        let _bsmod = br.get(3);
        let acmod = br.get(3) as u8;
        let mut dm = DownmixCodes::default();
        if (acmod & 1) != 0 && acmod != 1 {
            dm.cmixlev = Some(br.get(2) as u8);
        }
        if acmod & 4 != 0 {
            dm.surmixlev = Some(br.get(2) as u8);
        }
        if acmod == 2 {
            br.skip(2); // dsurmod
        }
        let lfeon = br.bit();
        let dialnorm = br.get(5) as u8;
        if br.bit() {
            br.skip(8); // compr
        }
        if br.bit() {
            br.skip(8); // langcod
        }
        if br.bit() {
            br.skip(7); // mixlevel, roomtyp
        }
        if acmod == 0 {
            br.skip(5); // dialnorm2
            if br.bit() {
                br.skip(8);
            }
            if br.bit() {
                br.skip(8);
            }
            if br.bit() {
                br.skip(7);
            }
        }
        br.skip(2); // copyrightb, origbs
        // timecod1e/timecod2e, or for bsid 6 (Annex D) xbsi1e/xbsi2e: both carry 14 bits.
        if br.bit() {
            if bsid == 6 {
                dm.dmixmod = Some(br.get(2) as u8);
                dm.ltrtcmixlev = Some(br.get(3) as u8);
                dm.ltrtsurmixlev = Some(br.get(3) as u8);
                dm.lorocmixlev = Some(br.get(3) as u8);
                dm.lorosurmixlev = Some(br.get(3) as u8);
            } else {
                br.skip(14);
            }
        }
        if br.bit() {
            br.skip(14);
        }
        if br.bit() {
            let addbsil = br.get(6) as usize;
            br.skip((addbsil + 1) * 8);
        }
        self.eac3 = false;
        self.strmtyp = 0;
        let shift = crate::header::ac3_rate_shift(bsid);
        self.set_config(acmod, lfeon, fscod, (fscod as usize, shift), 6, bsid);
        self.dialnorm = dialnorm;
        self.chanmap = None;
        self.downmix = dm;
        Ok(())
    }

    fn parse_bsi_eac3(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let strmtyp = br.get(2) as u8;
        let _substreamid = br.get(3);
        let _frmsiz = br.get(11);
        let fscod = br.get(2) as u8;
        let (numblkscod, hth_idx) = if fscod == 3 {
            let fscod2 = br.get(2) as usize;
            if fscod2 == 3 {
                return Err(DecodeError::Unsupported(Feature::ReservedSampleRate));
            }
            (3usize, fscod2)
        } else {
            (br.get(2) as usize, fscod as usize)
        };
        let num_blocks = [1, 2, 3, 6][numblkscod];
        let acmod = br.get(3) as u8;
        let lfeon = br.bit();
        let bsid = br.get(5) as u8;
        let dialnorm = br.get(5) as u8;
        if br.bit() {
            br.skip(8); // compr
        }
        if acmod == 0 {
            br.skip(5); // dialnorm2
            if br.bit() {
                br.skip(8); // compr2
            }
        }
        let mut chanmap = None;
        if strmtyp == 1 && br.bit() {
            chanmap = Some(br.get(16) as u16);
        }
        let mut dm = DownmixCodes::default();
        if br.bit() {
            // mixmdate
            if acmod > 2 {
                dm.dmixmod = Some(br.get(2) as u8);
            }
            if (acmod & 1) != 0 && acmod > 2 {
                dm.ltrtcmixlev = Some(br.get(3) as u8);
                dm.lorocmixlev = Some(br.get(3) as u8);
            }
            if acmod & 4 != 0 {
                dm.ltrtsurmixlev = Some(br.get(3) as u8);
                dm.lorosurmixlev = Some(br.get(3) as u8);
            }
            if lfeon && br.bit() {
                dm.lfemixlevcod = Some(br.get(5) as u8);
            }
            if strmtyp == 0 {
                if br.bit() {
                    br.skip(6); // pgmscl
                }
                if acmod == 0 && br.bit() {
                    br.skip(6); // pgmscl2
                }
                if br.bit() {
                    br.skip(6); // extpgmscl
                }
                match br.get(2) {
                    1 => br.skip(5),
                    2 => br.skip(12),
                    3 => {
                        // mixdeflen gives the length (2..=33 bytes) of the mixdata field,
                        // which begins with mixdeflen itself (Annex E 2.3.1.22, 3.10.4).
                        let start = br.pos();
                        let mixdeflen = br.get(5) as usize;
                        let end = start + 8 * (mixdeflen + 2);
                        br.skip(end - br.pos());
                    }
                    _ => {}
                }
                if acmod < 2 {
                    if br.bit() {
                        br.skip(14); // panmean, paninfo
                    }
                    if acmod == 0 && br.bit() {
                        br.skip(14); // panmean2, paninfo2
                    }
                }
                if br.bit() {
                    // frmmixcfginfoe
                    if numblkscod == 0 {
                        br.skip(5);
                    } else {
                        for _ in 0..num_blocks {
                            if br.bit() {
                                br.skip(5);
                            }
                        }
                    }
                }
            }
        }
        if br.bit() {
            // infomdate
            br.skip(5); // bsmod, copyrightb, origbs
            if acmod == 2 {
                br.skip(4); // dsurmod, dheadphonmod
            }
            if acmod >= 6 {
                br.skip(2); // dsurexmod
            }
            if br.bit() {
                br.skip(8); // mixlevel, roomtyp, adconvtyp
            }
            if acmod == 0 && br.bit() {
                br.skip(8);
            }
            if fscod < 3 {
                br.skip(1); // sourcefscod
            }
        }
        if strmtyp == 0 && numblkscod != 3 {
            br.skip(1); // convsync
        }
        if strmtyp == 2 {
            let blkid = if numblkscod == 3 { true } else { br.bit() };
            if blkid {
                br.skip(6); // frmsizecod
            }
        }
        if br.bit() {
            let addbsil = br.get(6) as usize;
            br.skip((addbsil + 1) * 8);
        }
        self.eac3 = true;
        self.strmtyp = strmtyp;
        self.set_config(acmod, lfeon, fscod, (hth_idx, 0), num_blocks, bsid);
        self.dialnorm = dialnorm;
        self.chanmap = chanmap;
        self.downmix = dm;
        Ok(())
    }

    fn set_config(
        &mut self,
        acmod: u8,
        lfeon: bool,
        fscod: u8,
        (hth_idx, rate_shift): (usize, u32),
        num_blocks: usize,
        bsid: u8,
    ) {
        let key = (acmod as u32) | (lfeon as u32) << 3 | (fscod as u32) << 4 | rate_shift << 6;
        if key != self.config_key {
            for d in self.delay.iter_mut() {
                d.fill(0.0);
            }
            self.config_key = key;
        }
        self.acmod = acmod;
        self.lfeon = lfeon;
        self.nfchans = NFCHANS[acmod as usize];
        self.fscod = fscod;
        self.hth_idx = hth_idx;
        self.rate_shift = rate_shift;
        self.num_blocks = num_blocks;
        self.bsid = bsid;
    }

    // --------------------------------------------------------------- audfrm

    fn parse_audfrm(
        &mut self,
        br: &mut BitReader<'_>,
        frame_words: usize,
        trace: &mut Trace,
    ) -> Result<(), E> {
        let nblk = self.num_blocks;
        let nfch = self.nfchans;
        let (expstre, ahte) = if nblk == 6 {
            (br.bit(), br.bit())
        } else {
            (true, false)
        };
        self.ahte = ahte;
        self.snroffststr = br.get(2) as u8;
        if self.snroffststr == 3 {
            return Err(bad("reserved snroffststr"));
        }
        if let Some(t) = trace.cur() {
            t.transproce_bit = Some(br.pos());
        }
        let transproce = br.bit();
        self.blkswe = br.bit();
        if let Some(t) = trace.cur() {
            t.dithflage_bit = Some(br.pos());
        }
        self.dithflage = br.bit();
        self.bamode = br.bit();
        self.frmfgaincode = br.bit();
        if let Some(t) = trace.cur() {
            t.dbaflde_bit = Some(br.pos());
        }
        self.dbaflde = br.bit();
        self.skipflde = br.bit();
        let spxattene = br.bit();
        // Coupling data.
        if self.acmod > 1 {
            self.cplstre_blk[0] = true;
            self.cplinu_blk[0] = br.bit();
            for blk in 1..nblk {
                self.cplstre_blk[blk] = br.bit();
                self.cplinu_blk[blk] = if self.cplstre_blk[blk] {
                    br.bit()
                } else {
                    self.cplinu_blk[blk - 1]
                };
            }
        } else {
            self.cplstre_blk = [false; 6];
            self.cplinu_blk = [false; 6];
        }
        let ncplblks = self.cplinu_blk[..nblk].iter().filter(|&&c| c).count();
        // Exponent strategies.
        if expstre {
            for blk in 0..nblk {
                self.expstr[CPL][blk] = if self.cplinu_blk[blk] {
                    br.get(2) as u8
                } else {
                    REUSE
                };
                for ch in 0..nfch {
                    self.expstr[ch][blk] = br.get(2) as u8;
                }
            }
        } else {
            self.features.frame_expstr = true;
            if self.acmod > 1 && ncplblks > 0 {
                let s = br.get(5) as usize;
                for blk in 0..6 {
                    self.expstr[CPL][blk] = FRM_EXPSTR[s][blk];
                }
            } else {
                self.expstr[CPL] = [REUSE; 6];
            }
            for ch in 0..nfch {
                let s = br.get(5) as usize;
                self.expstr[ch] = FRM_EXPSTR[s];
            }
        }
        if self.lfeon {
            for blk in 0..nblk {
                self.expstr[LFE][blk] = br.get(1) as u8;
            }
        }
        // Converter exponent strategy.
        if self.strmtyp == 0 {
            let convexpstre = if nblk != 6 { br.bit() } else { true };
            if convexpstre {
                br.skip(5 * nfch);
            }
        }
        // AHT.
        self.aht = [false; NCH];
        self.aht_read = [false; NCH];
        if ahte {
            let ncplregs = (0..6)
                .filter(|&b| self.cplstre_blk[b] || self.expstr[CPL][b] != REUSE)
                .count();
            if ncplblks == 6 && ncplregs == 1 {
                self.aht[CPL] = br.bit();
            }
            for ch in 0..nfch {
                let nregs = (0..6).filter(|&b| self.expstr[ch][b] != REUSE).count();
                if nregs == 1 {
                    self.aht[ch] = br.bit();
                }
            }
            if self.lfeon {
                let nregs = (0..6).filter(|&b| self.expstr[LFE][b] != REUSE).count();
                if nregs == 1 {
                    self.aht[LFE] = br.bit();
                }
            }
            self.features.aht |= self.aht.iter().any(|&a| a);
        }
        if self.snroffststr == 0 {
            self.frmcsnroffst = br.get(6) as u8;
            self.frmfsnroffst = br.get(4) as u8;
        }
        if let Some(t) = trace.cur() {
            t.transproc_data_bit = Some(br.pos());
        }
        if transproce {
            // Transient pre-noise processing data: parsed, not applied (FFmpeg 9.0.2 does
            // not apply it either; see research/d4/ac3/STATUS.md).
            self.features.tpnp = true;
            for _ in 0..nfch {
                if br.bit() {
                    br.skip(18); // transprocloc, transproclen
                }
            }
        }
        self.chinspxatten = [false; 5];
        if spxattene {
            for ch in 0..nfch {
                self.chinspxatten[ch] = br.bit();
                if self.chinspxatten[ch] {
                    self.spxattencod[ch] = br.get(5) as u8;
                }
            }
        }
        let blkstrtinfoe = if nblk != 1 { br.bit() } else { false };
        if blkstrtinfoe {
            let mut lg = 0usize;
            while (1usize << lg) < frame_words {
                lg += 1;
            }
            br.skip((nblk - 1) * (4 + lg));
        }
        self.firstspxcos = [true; 5];
        self.firstcplcos = [true; 5];
        self.firstcplleak = true;
        self.cplbndstrc_set_in_frame = false;
        self.spxbndstrc[..17].copy_from_slice(&DEF_SPX_BNDSTRC);
        Ok(())
    }

    // ---------------------------------------------------------------- frame

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
                if !self.ffmpeg_compat {
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

    // ---------------------------------------------------------------- block

    fn parse_block(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        rng: &mut Rng,
        noise: NoiseMode,
        groups: &GroupTables,
        trace: &mut Trace,
    ) -> Result<(), E> {
        let nfch = self.nfchans;
        let eac3 = self.eac3;
        let acmod = self.acmod;

        // Block switch and dither flags.
        if !eac3 || self.blkswe {
            for ch in 0..nfch {
                self.blksw[ch] = br.bit();
            }
        } else {
            self.blksw = [false; 5];
        }
        if let Some(t) = trace.cur() {
            t.dithflag_insert_bits.push(br.pos());
        }
        if !eac3 || self.dithflage {
            for ch in 0..nfch {
                if let Some(t) = trace.cur() {
                    t.dithflag_bits.push(br.pos());
                }
                self.dithflag[ch] = br.bit();
            }
        } else {
            self.dithflag = [true; 5];
        }
        self.features.block_switch |= self.blksw[..nfch].iter().any(|&b| b);
        self.features.dither |= self.dithflag[..nfch].iter().any(|&b| b);

        // Dynamic range.
        if br.bit() {
            self.dynrng[0] = dynrng_gain(br.get(8));
            self.features.dynrng = true;
        }
        if acmod == 0 && br.bit() {
            self.dynrng[1] = dynrng_gain(br.get(8));
        }

        // Spectral extension strategy and coordinates (E-AC-3).
        if eac3 {
            let spxstre = blk == 0 || br.bit();
            if spxstre {
                self.parse_spx_strategy(br)?;
            }
            if self.spxinu {
                self.parse_spx_coords(br, rng.compatible());
            }
        }

        // Coupling strategy.
        let cplstre = if eac3 {
            self.cplstre_blk[blk]
        } else {
            br.bit()
        };
        let mut new_cpl_strategy = false;
        if cplstre {
            let cplinu = if eac3 { self.cplinu_blk[blk] } else { br.bit() };
            new_cpl_strategy = true;
            self.cplinu = cplinu;
            if cplinu {
                if let Some(t) = trace.cur() {
                    t.ecplinu_bits.push(br.pos());
                }
                self.parse_cpl_strategy(br)?;
            } else {
                self.chincpl = [false; 5];
                self.firstcplcos = [true; 5];
                self.firstcplleak = true;
                self.phsflginu = false;
            }
        } else if !eac3 && blk == 0 {
            return Err(bad("coupling strategy missing in block 0"));
        }
        let cplinu = self.cplinu;
        self.features.coupling |= cplinu;

        // Coupling coordinates and phase flags.
        if cplinu {
            let mut cplcoe0 = [false; 2];
            for ch in 0..nfch {
                if self.chincpl[ch] {
                    let cplcoe = if eac3 && self.firstcplcos[ch] {
                        self.firstcplcos[ch] = false;
                        true
                    } else {
                        br.bit()
                    };
                    if cplcoe {
                        if ch < 2 {
                            cplcoe0[ch] = true;
                        }
                        let mstrcplco = br.get(2) as usize;
                        for bnd in 0..self.ncplbnd {
                            let e = br.get(4) as usize;
                            let m = br.get(4) as f32;
                            let t = if e == 15 { m / 16.0 } else { (m + 16.0) / 32.0 };
                            let sh = e + 3 * mstrcplco;
                            self.cplco[ch][bnd] = t * EXP_SCALE[sh.min(31)];
                        }
                    }
                } else if eac3 {
                    self.firstcplcos[ch] = true;
                }
            }
            if let Some(t) = trace.cur() {
                t.blocks.push(BlockTrace {
                    blk,
                    cplstre: new_cpl_strategy,
                    phsflginu_bit: self.phsflginu_pos,
                    phsflg_bit: br.pos(),
                    ncplbnd: self.ncplbnd,
                    cplcoe_any: cplcoe0[0] || cplcoe0[1],
                    dba_bit: 0,
                });
            }
            if acmod == 2 && self.phsflginu && (cplcoe0[0] || cplcoe0[1]) {
                for bnd in 0..self.ncplbnd {
                    self.phsflg[bnd] = br.bit();
                }
                self.features.phase_flags = true;
            }
        }

        // Rematrixing.
        if acmod == 2 {
            let rematstr = if eac3 { blk == 0 || br.bit() } else { br.bit() };
            if rematstr {
                self.nrematbd = self.rematrix_bands();
                for b in 0..self.nrematbd {
                    self.rematflg[b] = br.bit();
                }
            } else if !eac3 && blk == 0 {
                self.nrematbd = 0;
            }
            if self.rematflg[..self.nrematbd].iter().any(|&f| f) {
                self.features.rematrix = true;
            }
        }

        // Exponent strategies (AC-3 carries them per block).
        if !eac3 {
            self.expstr[CPL][blk] = if cplinu { br.get(2) as u8 } else { REUSE };
            for ch in 0..nfch {
                self.expstr[ch][blk] = br.get(2) as u8;
            }
            if self.lfeon {
                self.expstr[LFE][blk] = br.get(1) as u8;
            }
        }
        if blk == 0 {
            for ch in self.coded_channels().collect::<ArrayVec6>().iter() {
                if self.expstr[*ch][0] == REUSE && !eac3 {
                    return Err(bad("exponent reuse in block 0"));
                }
            }
        }
        // Channel bandwidth codes.
        for ch in 0..nfch {
            if self.expstr[ch][blk] != REUSE {
                if self.chincpl[ch] && cplinu {
                    self.endmant[ch] = self.cplstrtmant;
                } else if eac3 && self.spxinu && self.chinspx[ch] {
                    self.endmant[ch] = spx_band_start(self.spx_begin);
                } else {
                    let chbwcod = br.get(6) as usize;
                    if chbwcod > 60 {
                        return Err(bad("chbwcod > 60"));
                    }
                    self.endmant[ch] = (chbwcod + 12) * 3 + 37;
                }
                self.strtmant[ch] = 0;
            }
        }

        // Exponents.
        if cplinu && self.expstr[CPL][blk] != REUSE {
            let strategy = self.expstr[CPL][blk];
            let cplabsexp = (br.get(4) as u8) << 1;
            let gs = grp_size(strategy);
            let ngrps = (self.cplendmant - self.cplstrtmant) / (3 * gs);
            let start = self.cplstrtmant;
            decode_exponents(br, &mut self.exps[CPL], cplabsexp, start, ngrps, gs)?;
            self.strtmant[CPL] = self.cplstrtmant;
            self.endmant[CPL] = self.cplendmant;
            self.ba_dirty[CPL] = 3;
        } else if cplinu && new_cpl_strategy && !eac3 && self.expstr[CPL][blk] == REUSE && blk == 0
        {
            return Err(bad("coupling exponents missing"));
        }
        for ch in 0..nfch {
            let strategy = self.expstr[ch][blk];
            if strategy != REUSE {
                let absexp = br.get(4) as u8;
                let gs = grp_size(strategy);
                let end = self.endmant[ch];
                let ngrps = (end - 1 + 3 * gs - 3) / (3 * gs);
                self.exps[ch][0] = absexp;
                decode_exponents(br, &mut self.exps[ch], absexp, 1, ngrps, gs)?;
                br.skip(2); // gainrng
                self.ba_dirty[ch] = 3;
            }
        }
        if self.lfeon && self.expstr[LFE][blk] != REUSE {
            let absexp = br.get(4) as u8;
            self.exps[LFE][0] = absexp;
            decode_exponents(br, &mut self.exps[LFE], absexp, 1, 2, 1)?;
            self.strtmant[LFE] = 0;
            self.endmant[LFE] = 7;
            self.ba_dirty[LFE] = 3;
        }

        // Bit allocation parametric information.
        // Lowest bit-allocation stage that must be re-run for every channel.
        let mut all_dirty = 0u8;
        if !eac3 || self.bamode {
            if br.bit() {
                self.ba = BaParams {
                    sdcycod: br.get(2) as u8,
                    fdcycod: br.get(2) as u8,
                    sgaincod: br.get(2) as u8,
                    dbpbcod: br.get(2) as u8,
                    floorcod: br.get(3) as u8,
                };
                all_dirty = all_dirty.max(2);
            } else if blk == 0 && !eac3 {
                return Err(bad("bit allocation info missing in block 0"));
            }
        } else {
            let def = BaParams {
                sdcycod: 2,
                fdcycod: 1,
                sgaincod: 1,
                dbpbcod: 2,
                floorcod: 7,
            };
            if self.ba != def {
                self.ba = def;
                all_dirty = all_dirty.max(2);
            }
        }
        if !eac3 {
            if br.bit() {
                self.csnroffst = br.get(6) as u8;
                if cplinu {
                    self.fsnroffst[CPL] = br.get(4) as u8;
                    self.fgaincod[CPL] = br.get(3) as u8;
                }
                for ch in 0..nfch {
                    self.fsnroffst[ch] = br.get(4) as u8;
                    self.fgaincod[ch] = br.get(3) as u8;
                }
                if self.lfeon {
                    self.fsnroffst[LFE] = br.get(4) as u8;
                    self.fgaincod[LFE] = br.get(3) as u8;
                }
                all_dirty = all_dirty.max(2);
            } else if blk == 0 {
                return Err(bad("SNR offsets missing in block 0"));
            }
        } else {
            if self.snroffststr == 0 {
                if blk == 0 || self.csnroffst != self.frmcsnroffst {
                    all_dirty = all_dirty.max(1);
                }
                self.csnroffst = self.frmcsnroffst;
                let f = self.frmfsnroffst;
                for v in self.fsnroffst.iter_mut() {
                    if *v != f {
                        all_dirty = all_dirty.max(1);
                    }
                    *v = f;
                }
            } else {
                let snroffste = blk == 0 || br.bit();
                if snroffste {
                    self.csnroffst = br.get(6) as u8;
                    if self.snroffststr == 1 {
                        let f = br.get(4) as u8;
                        self.fsnroffst = [f; NCH];
                    } else {
                        if cplinu {
                            self.fsnroffst[CPL] = br.get(4) as u8;
                        }
                        for ch in 0..nfch {
                            self.fsnroffst[ch] = br.get(4) as u8;
                        }
                        if self.lfeon {
                            self.fsnroffst[LFE] = br.get(4) as u8;
                        }
                    }
                    all_dirty = all_dirty.max(1);
                }
            }
            let fgaincode = self.frmfgaincode && br.bit();
            let mut new = [4u8; NCH];
            if fgaincode {
                if cplinu {
                    new[CPL] = br.get(3) as u8;
                }
                for v in new.iter_mut().take(nfch) {
                    *v = br.get(3) as u8;
                }
                if self.lfeon {
                    new[LFE] = br.get(3) as u8;
                }
            }
            for ch in 0..NCH {
                if self.fgaincod[ch] != new[ch] {
                    self.fgaincod[ch] = new[ch];
                    self.ba_dirty[ch] = self.ba_dirty[ch].max(2);
                }
            }
            if self.strmtyp == 0 && br.bit() {
                br.skip(10); // convsnroffst
            }
        }
        if cplinu {
            let cplleake = if eac3 && self.firstcplleak {
                self.firstcplleak = false;
                true
            } else {
                br.bit()
            };
            if cplleake {
                self.cplfleak = br.get(3) as u8;
                self.cplsleak = br.get(3) as u8;
                self.ba_dirty[CPL] = self.ba_dirty[CPL].max(2);
            } else if !eac3 && new_cpl_strategy && blk == 0 {
                return Err(bad("coupling leak info missing"));
            }
        }

        // Delta bit allocation.
        if let Some(t) = trace.cur() {
            if t.blocks.last().map(|b| b.blk) != Some(blk) {
                t.blocks.push(BlockTrace {
                    blk,
                    ..Default::default()
                });
            }
            if let Some(b) = t.blocks.last_mut() {
                b.dba_bit = br.pos();
            }
        }
        if (!eac3 || self.dbaflde) && br.bit() {
            {
                self.features.delta_ba = true;
                let mut mode = [0u8; NCH];
                if cplinu {
                    mode[CPL] = br.get(2) as u8;
                }
                for ch in 0..nfch {
                    mode[ch] = br.get(2) as u8;
                }
                let order: [usize; 6] = [CPL, 0, 1, 2, 3, 4];
                for &ch in order.iter() {
                    if ch == CPL && !cplinu {
                        continue;
                    }
                    if ch != CPL && ch >= nfch {
                        continue;
                    }
                    match mode[ch] {
                        0 => {}
                        1 => {
                            let d = &mut self.dba[ch];
                            d.active = true;
                            d.nseg = br.get(3) as usize;
                            for seg in 0..=d.nseg {
                                d.offst[seg] = br.get(5) as u8;
                                d.len[seg] = br.get(4) as u8;
                                d.ba[seg] = br.get(3) as u8;
                            }
                        }
                        2 => self.dba[ch].active = false,
                        _ => return Err(bad("reserved deltbae")),
                    }
                }
                all_dirty = all_dirty.max(2);
            }
        }

        // Skip field.
        if (!eac3 || self.skipflde) && br.bit() {
            let skipl = br.get(9) as usize;
            br.skip(skipl * 8);
            self.features.skip_fields = true;
        }

        for d in self.ba_dirty.iter_mut() {
            *d = (*d).max(all_dirty);
        }
        self.run_bit_allocation(cplinu)?;

        self.unpack_mantissas(br, blk, rng, noise, groups)
    }

    fn unpack_mantissas(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        rng: &mut Rng,
        noise: NoiseMode,
        groups: &GroupTables,
    ) -> Result<(), E> {
        self.coef_clear();
        let mut g = Groups::new();
        let mut got_cpl = false;
        for ch in 0..self.nfchans {
            if self.aht[ch] {
                if !self.aht_read[ch] {
                    self.read_aht_channel(br, ch, rng, noise)?;
                    self.aht_read[ch] = true;
                }
                self.apply_aht(ch, blk);
            } else {
                let (dith, end) = (
                    self.dithflag[ch] && noise == NoiseMode::Normal,
                    self.endmant[ch],
                );
                read_mantissas(
                    br,
                    &self.bap[ch],
                    &self.exps[ch],
                    0,
                    end,
                    dith,
                    rng,
                    groups,
                    &mut g,
                    &mut self.icoef[ch],
                );
            }
            if self.cplinu && self.chincpl[ch] && !got_cpl {
                got_cpl = true;
                if self.aht[CPL] {
                    if !self.aht_read[CPL] {
                        self.read_aht_channel(br, CPL, rng, noise)?;
                        self.aht_read[CPL] = true;
                    }
                    self.apply_aht(CPL, blk);
                } else {
                    let (s, e) = (self.cplstrtmant, self.cplendmant);
                    // Compatible coupling draws once here, even if every channel
                    // suppresses the resulting noise during decoupling.
                    read_mantissas(
                        br,
                        &self.bap[CPL],
                        &self.exps[CPL],
                        s,
                        e,
                        rng.compatible(),
                        rng,
                        groups,
                        &mut g,
                        &mut self.icoef[CPL],
                    );
                }
            }
        }
        if self.lfeon {
            if self.aht[LFE] {
                if !self.aht_read[LFE] {
                    self.read_aht_channel(br, LFE, rng, noise)?;
                    self.aht_read[LFE] = true;
                }
                self.apply_aht(LFE, blk);
            } else {
                read_mantissas(
                    br,
                    &self.bap[LFE],
                    &self.exps[LFE],
                    0,
                    7,
                    false,
                    rng,
                    groups,
                    &mut g,
                    &mut self.icoef[LFE],
                );
            }
        }
        Ok(())
    }

    fn rematrix_bands(&self) -> usize {
        if self.cplinu {
            if self.cplbegf == 0 {
                2
            } else if self.cplbegf < 3 {
                3
            } else {
                4
            }
        } else if self.eac3 && self.spxinu {
            if self.spxbegf < 2 { 3 } else { 4 }
        } else {
            4
        }
    }

    fn parse_spx_strategy(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        self.spxinu = br.bit();
        if !self.spxinu {
            self.chinspx = [false; 5];
            self.firstspxcos = [true; 5];
            return Ok(());
        }
        self.features.spx = true;
        if self.acmod == 1 {
            self.chinspx[0] = true;
        } else {
            for ch in 0..self.nfchans {
                self.chinspx[ch] = br.bit();
            }
        }
        self.spxstrtf = br.get(2) as usize;
        let spxbegf = br.get(3) as usize;
        let spxendf = br.get(3) as usize;
        self.spxbegf = spxbegf;
        self.spx_begin = if spxbegf < 6 {
            spxbegf + 2
        } else {
            spxbegf * 2 - 3
        };
        self.spx_end = if spxendf < 3 {
            spxendf + 5
        } else {
            spxendf * 2 + 3
        };
        if self.spx_begin >= self.spx_end {
            return Err(bad("spx begin >= end"));
        }
        if self.spxstrtf >= self.spx_begin {
            return Err(bad("spx copy start >= begin"));
        }
        if br.bit() {
            for bnd in self.spx_begin + 1..self.spx_end {
                self.spxbndstrc[bnd] = br.bit();
            }
        }
        // Band sizes (Annex E 3.6.2).
        self.nspxbnds = 1;
        self.spxbndsz[0] = 12;
        for bnd in self.spx_begin + 1..self.spx_end {
            if !self.spxbndstrc[bnd] {
                self.spxbndsz[self.nspxbnds] = 12;
                self.nspxbnds += 1;
            } else {
                self.spxbndsz[self.nspxbnds - 1] += 12;
            }
        }
        Ok(())
    }

    fn parse_spx_coords(&mut self, br: &mut BitReader<'_>, compatible_noise: bool) {
        for ch in 0..self.nfchans {
            if !self.chinspx[ch] {
                self.firstspxcos[ch] = true;
                continue;
            }
            let spxcoe = if self.firstspxcos[ch] {
                self.firstspxcos[ch] = false;
                true
            } else {
                br.bit()
            };
            if !spxcoe {
                continue;
            }
            let spxblnd = br.get(5) as f32;
            let mstrspxco = br.get(2) as usize;
            let noffset = spxblnd / 32.0;
            let spx_end_mant = spx_band_start(self.spx_end) as f32;
            let mut spxmant = spx_band_start(self.spx_begin);
            for bnd in 0..self.nspxbnds {
                let e = br.get(4) as usize;
                let m = br.get(2) as f32;
                let t = if e == 15 { m / 4.0 } else { (m + 4.0) / 8.0 };
                self.spxco[ch][bnd] = t * EXP_SCALE[(e + 3 * mstrspxco).min(31)];
                // Annex E 3.6.4.2.1 blending factors.
                let bandsize = self.spxbndsz[bnd];
                let mut nratio = (spxmant as f32 + 0.5 * bandsize as f32) / spx_end_mant - noffset;
                nratio = nratio.clamp(0.0, 1.0);
                self.nblend[ch][bnd] = if compatible_noise {
                    (3.0 * nratio).sqrt()
                } else {
                    nratio.sqrt()
                };
                self.sblend[ch][bnd] = (1.0 - nratio).sqrt();
                spxmant += bandsize;
            }
        }
    }

    fn parse_cpl_strategy(&mut self, br: &mut BitReader<'_>) -> Result<(), E> {
        let eac3 = self.eac3;
        if eac3 && br.bit() {
            self.features.ecpl = true;
            return Err(DecodeError::Unsupported(Feature::EnhancedCoupling));
        }
        if eac3 && self.acmod == 2 {
            self.chincpl[0] = true;
            self.chincpl[1] = true;
        } else {
            for ch in 0..self.nfchans {
                self.chincpl[ch] = br.bit();
            }
        }
        for ch in self.nfchans..5 {
            self.chincpl[ch] = false;
        }
        self.phsflginu_pos = if self.acmod == 2 {
            Some(br.pos())
        } else {
            None
        };
        self.phsflginu = if self.acmod == 2 { br.bit() } else { false };
        self.cplbegf = br.get(4) as i32;
        if eac3 && self.spxinu {
            let s = self.spxbegf as i32;
            self.cplendf = if s < 6 { s - 2 } else { s * 2 - 7 };
        } else {
            self.cplendf = br.get(4) as i32;
        }
        let ncplsubnd = 3 + self.cplendf - self.cplbegf;
        if ncplsubnd < 1 {
            return Err(bad("coupling end before begin"));
        }
        let ncplsubnd = ncplsubnd as usize;
        let begf = self.cplbegf as usize;
        if begf + ncplsubnd > 18 {
            return Err(bad("coupling range"));
        }
        self.ncplsubnd = ncplsubnd;
        let read_strc = if eac3 { br.bit() } else { true };
        if read_strc {
            for bnd in 1..ncplsubnd {
                self.cplbndstrc[begf + bnd] = br.bit();
            }
            self.cplbndstrc_set_in_frame = true;
        } else if !self.cplbndstrc_set_in_frame {
            self.cplbndstrc = DEF_CPL_BNDSTRC;
            self.cplbndstrc_set_in_frame = true;
        }
        let mut nb = 0usize;
        for sb in 0..ncplsubnd {
            if sb > 0 && !self.cplbndstrc[begf + sb] {
                nb += 1;
            }
            self.cpl_sb_band[sb] = nb as u8;
        }
        self.ncplbnd = nb + 1;
        self.cplstrtmant = begf * 12 + 37;
        self.cplendmant = ((self.cplendf + 3) * 12 + 37) as usize;
        Ok(())
    }

    fn run_bit_allocation(&mut self, cplinu: bool) -> Result<(), E> {
        let all_zero_snr = self.csnroffst == 0
            && self.coded_channels().all(|ch| self.fsnroffst[ch] == 0)
            && (!cplinu || self.fsnroffst[CPL] == 0);
        let chans: [usize; 7] = [0, 1, 2, 3, 4, CPL, LFE];
        for &ch in chans.iter() {
            let present = if ch == CPL {
                cplinu
            } else if ch == LFE {
                self.lfeon
            } else {
                ch < self.nfchans
            };
            let level = self.ba_dirty[ch];
            if !present || level == 0 {
                continue;
            }
            self.ba_dirty[ch] = 0;
            // An AHT channel's allocation is fixed once its mantissas were read (block 0).
            if self.aht[ch] && self.aht_read[ch] {
                continue;
            }
            let (start, end) = (self.strtmant[ch], self.endmant[ch]);
            if start >= end || end > 253 {
                // E.g. reused exponents that were never transmitted (corrupt stream).
                self.ba_dirty[ch] = 3;
                return Err(bad("bit allocation range"));
            }
            if all_zero_snr {
                self.bap[ch][start..end].fill(0);
                // The cached stages are stale for the next block.
                self.ba_dirty[ch] = 3;
                continue;
            }
            let cache = &mut self.ba_cache[ch];
            if level >= 3 {
                bitalloc::stage_psd(&self.exps[ch], start, end, cache).map_err(bad)?;
            }
            if level >= 2 {
                let p = MaskParams {
                    start,
                    end,
                    fscod: self.hth_idx,
                    rate_shift: self.rate_shift,
                    params: self.ba,
                    fgain: FASTGAIN[self.fgaincod[ch] as usize],
                    cpl_leak: (ch == CPL).then_some((self.cplfleak, self.cplsleak)),
                    dba: &self.dba[ch],
                };
                bitalloc::stage_mask(&p, cache).map_err(bad)?;
            }
            let snroffset =
                ((((self.csnroffst as i32) - 15) << 4) + self.fsnroffst[ch] as i32) << 2;
            bitalloc::stage_bap(
                cache,
                start,
                end,
                snroffset,
                self.ba.floorcod,
                self.aht[ch],
                &mut self.bap[ch],
            );
        }
        Ok(())
    }

    /// Analysis aid (feature `debug-dump`, never enabled in production builds): writes
    /// per-block coefficients/delay lines to $AC3DUMP and a JSON summary line to stderr
    /// when $AC3J is set. Used by the research/d4/ac3 comparison scripts.
    #[cfg(feature = "debug-dump")]
    fn debug_dump_block(&self) {
        if std::env::var_os("AC3J").is_some() {
            let baps: Vec<Vec<u8>> = (0..NCH).map(|c| self.bap[c].to_vec()).collect();
            let exps: Vec<Vec<u8>> = (0..NCH).map(|c| self.exps[c].to_vec()).collect();
            let spx = if self.spxinu {
                (spx_band_start(self.spx_begin), spx_band_start(self.spx_end))
            } else {
                (0, 0)
            };
            eprintln!(
                "J {{\"remat\":{:?},\"nremat\":{},\"end\":{:?},\"spx\":[{},{}],\"chinspx\":{:?},\"aht\":{:?},\"cpl\":{},\"cplrange\":[{},{}],\"bap\":{:?},\"exps\":{:?},\"spxstrtf\":{},\"spxbndsz\":{:?},\"spxco\":{:?},\"nblend\":{:?},\"sblend\":{:?},\"atten\":{:?},\"attencod\":{:?},\"dyn\":{:?},\"blksw\":{:?}}}",
                self.rematflg,
                self.nrematbd,
                self.endmant,
                spx.0,
                spx.1,
                self.chinspx,
                self.aht,
                self.cplinu,
                self.cplstrtmant,
                self.cplendmant,
                baps,
                exps,
                spx_band_start(self.spxstrtf),
                &self.spxbndsz[..self.nspxbnds],
                self.spxco,
                self.nblend,
                self.sblend,
                self.chinspxatten,
                self.spxattencod,
                self.dynrng,
                self.blksw
            );
        }
        if let Some(path) = std::env::var_os("AC3DUMP") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
            {
                for ch in 0..NCH {
                    for v in self.coef[ch].iter().chain(self.delay[ch].iter()) {
                        let _ = f.write_all(&v.to_le_bytes());
                    }
                }
            }
        }
    }

    fn coef_clear(&mut self) {
        let n = self.nfchans;
        for c in self.icoef[..n].iter_mut() {
            c.fill(0);
        }
        if self.cplinu {
            self.icoef[CPL].fill(0);
        }
        if self.lfeon {
            self.icoef[LFE].fill(0);
        }
    }

    // ------------------------------------------------------------------ AHT

    fn read_aht_channel(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        rng: &mut Rng,
        noise: NoiseMode,
    ) -> Result<(), E> {
        let (start, end) = (self.strtmant[ch], self.endmant[ch]);
        let gaqmod = br.get(2) as u8;
        let endbap = if gaqmod < 2 { 12 } else { 17 };
        let hebap = &self.bap[ch];
        let mut active = 0usize;
        for bin in start..end {
            let h = hebap[bin];
            if h > 7 && h < endbap {
                active += 1;
            }
        }
        let nsections = match gaqmod {
            1 | 2 => active,
            3 => active.div_ceil(3),
            _ => 0,
        };
        if gaqmod != 0 {
            self.features.gaq = true;
        }
        // Gains in active-bin order; stored as log2 gain (0, 1, 2).
        let mut gains = [0u8; 256];
        match gaqmod {
            1 | 2 => {
                let g = if gaqmod == 1 { 1 } else { 2 };
                for v in gains.iter_mut().take(nsections) {
                    *v = if br.bit() { g } else { 0 };
                }
            }
            3 => {
                for s in 0..nsections {
                    let grp = br.get(5) as usize;
                    let m = [grp / 9, (grp % 9) / 3, grp % 3];
                    for (k, &mv) in m.iter().enumerate() {
                        if 3 * s + k < 256 {
                            gains[3 * s + k] = (mv.min(2)) as u8;
                        }
                    }
                }
            }
            _ => {}
        }
        let idct = &AHT_IDCT;
        let mut active_idx = 0usize;
        for bin in start..end {
            let h = hebap[bin] as usize;
            let mut x = [0f32; 6];
            if h == 0 {
                // No bits. FFmpeg 9.0.2 fills these DCT coefficients with uniform noise in
                // [-0.5, 0.5) independent of dithflag (black-box, STATUS.md).
                if noise == NoiseMode::Normal {
                    for xj in x.iter_mut() {
                        *xj = rng.aht_coefficient();
                    }
                }
            } else if h < 8 {
                let bits = HEBAP_BITS[h] as u32;
                let idx = br.get(bits) as usize;
                let row: &[i16; 6] = match h {
                    1 => &VQ_HEBAP1[idx],
                    2 => &VQ_HEBAP2[idx],
                    3 => &VQ_HEBAP3[idx],
                    // FFmpeg 9.0.2 compatibility (black-box observation, STATUS.md): FFmpeg
                    // decodes hebap-4 index i with Table E4.4 row i + 1 and index 31 as zero.
                    4 if self.ffmpeg_compat => VQ_HEBAP4.get(idx + 1).unwrap_or(&[0; 6]),
                    4 => &VQ_HEBAP4[idx],
                    5 => &VQ_HEBAP5[idx],
                    6 => &VQ_HEBAP6[idx],
                    _ => &VQ_HEBAP7[idx],
                };
                for j in 0..6 {
                    x[j] = row[j] as f32 / 32768.0;
                }
            } else {
                let m = HEBAP_BITS[h] as u32;
                let gain_log = if h < endbap as usize {
                    let g = gains[active_idx];
                    active_idx += 1;
                    g
                } else {
                    0
                };
                for xj in x.iter_mut() {
                    *xj = gaq_mantissa(br, h, m, gain_log);
                }
            }
            for (blk, row) in idct.iter().enumerate() {
                let mut s = 0f32;
                for j in 0..6 {
                    s += row[j] * x[j];
                }
                self.aht_coef[ch][blk][bin] = s * 8388608.0;
            }
        }
        Ok(())
    }

    fn apply_aht(&mut self, ch: usize, blk: usize) {
        let (start, end) = (self.strtmant[ch], self.endmant[ch]);
        let src = &self.aht_coef[ch][blk];
        for bin in start..end {
            self.icoef[ch][bin] = (src[bin].round() as i32) >> self.exps[ch][bin];
        }
    }

    // ------------------------------------------------------- reconstruction

    fn reconstruct(&mut self, rng: &mut Rng, noise: NoiseMode) {
        let nfch = self.nfchans;
        // Decoupling (7.4.3).
        if self.cplinu {
            let begf = self.cplbegf as usize;
            let shared_noise = rng.compatible();
            for ch in 0..nfch {
                if !self.chincpl[ch] {
                    continue;
                }
                let dith = self.dithflag[ch] && noise == NoiseMode::Normal && !self.aht[CPL];
                for sb in 0..self.ncplsubnd {
                    let bnd = self.cpl_sb_band[sb] as usize;
                    let co = self.cplco[ch][bnd] as f64 * 8.0;
                    // Phase flags negate after the coordinate product (matters for the
                    // rounding of the Q23 result; matches FFmpeg 9.0.2).
                    let flip = ch == 1 && self.acmod == 2 && self.phsflginu && self.phsflg[bnd];
                    let lo = (begf + sb) * 12 + 37;
                    for bin in lo..lo + 12 {
                        let v = if self.bap[CPL][bin] != 0 {
                            self.icoef[CPL][bin]
                        } else if shared_noise {
                            if self.dithflag[ch] {
                                self.icoef[CPL][bin]
                            } else {
                                0
                            }
                        } else if self.aht[CPL] {
                            self.icoef[CPL][bin]
                        } else if dith {
                            rng.dither_q23() >> self.exps[CPL][bin]
                        } else {
                            0
                        };
                        let p = (v as f64 * co).floor() as i32;
                        self.icoef[ch][bin] = if flip { -p } else { p };
                    }
                }
            }
        }
        // Rematrixing (7.5.4), limited to the lower bandwidth of the two channels.
        if self.acmod == 2 && self.nrematbd > 0 {
            let upper = self.endmant[0].min(self.endmant[1]);
            let (l, r) = self.icoef.split_at_mut(1);
            let (l, r) = (&mut l[0], &mut r[0]);
            for b in 0..self.nrematbd {
                if !self.rematflg[b] {
                    continue;
                }
                let lo = REMAT_BANDS[b];
                let hi = REMAT_BANDS[b + 1].min(upper);
                for bin in lo..hi {
                    let (a, c) = (l[bin], r[bin]);
                    l[bin] = a + c;
                    r[bin] = a - c;
                }
            }
        }
        // Conversion to float with the dynamic range gain (7.7.1) of each channel.
        let g0 = self.dynrng[0] * Q23_SCALE;
        let g1 = if self.acmod == 0 {
            self.dynrng[1] * Q23_SCALE
        } else {
            g0
        };
        for ch in 0..nfch {
            let g = if ch == 1 { g1 } else { g0 };
            for (d, &s) in self.coef[ch].iter_mut().zip(self.icoef[ch].iter()) {
                *d = s as f32 * g;
            }
        }
        if self.lfeon {
            for (d, &s) in self.coef[LFE].iter_mut().zip(self.icoef[LFE].iter()) {
                *d = s as f32 * g0;
            }
        }
        // Spectral extension (Annex E 3.6).
        if self.eac3 && self.spxinu {
            for ch in 0..nfch {
                if self.chinspx[ch] {
                    self.apply_spx(ch, rng, noise);
                }
            }
        }
    }

    fn apply_spx(&mut self, ch: usize, rng: &mut Rng, noise: NoiseMode) {
        let tc = &mut self.coef[ch];
        let copystart = spx_band_start(self.spxstrtf);
        let copyend = spx_band_start(self.spx_begin);
        let mut copy = copystart;
        let mut insert = copyend;
        let mut wrap = [false; 18];
        for bnd in 0..self.nspxbnds {
            let bandsize = self.spxbndsz[bnd];
            if copy + bandsize > copyend {
                copy = copystart;
                wrap[bnd] = true;
            }
            for _ in 0..bandsize {
                if copy == copyend {
                    copy = copystart;
                }
                tc[insert] = tc[copy];
                insert += 1;
                copy += 1;
            }
        }
        // Banded RMS energy of the translated coefficients.
        let mut rms = [0f32; 18];
        let mut m = copyend;
        for bnd in 0..self.nspxbnds {
            let bs = self.spxbndsz[bnd];
            let acc: f32 = tc[m..m + bs].iter().map(|v| v * v).sum();
            rms[bnd] = (acc / bs as f32).sqrt();
            m += bs;
        }
        // Band border notch filter.
        if self.chinspxatten[ch] {
            let code = self.spxattencod[ch] as usize;
            let att = [spx_atten(code, 0), spx_atten(code, 1), spx_atten(code, 2)];
            let notch = |tc: &mut [f32; 256], border: usize| {
                tc[border - 2] *= att[0];
                tc[border - 1] *= att[1];
                tc[border] *= att[2];
                tc[border + 1] *= att[1];
                tc[border + 2] *= att[0];
            };
            notch(tc, copyend);
            let mut border = copyend + self.spxbndsz[0];
            for bnd in 1..self.nspxbnds {
                if wrap[bnd] {
                    notch(tc, border);
                }
                border += self.spxbndsz[bnd];
            }
            self.features.spx_atten = true;
        }
        // Noise blending and coordinate scaling.
        let mut m = copyend;
        for bnd in 0..self.nspxbnds {
            let bs = self.spxbndsz[bnd];
            let co = self.spxco[ch][bnd] * 32.0;
            let band = &mut tc[m..m + bs];
            if rng.compatible() {
                let signal = self.sblend[ch][bnd] * co;
                let amplitude = (self.nblend[ch][bnd] * co) * rms[bnd];
                for v in band {
                    *v = *v * signal + rng.compatible_spx_noise(amplitude);
                }
            } else {
                let nscale = rms[bnd] * self.nblend[ch][bnd];
                let sscale = self.sblend[ch][bnd];
                for v in band {
                    let n = if noise == NoiseMode::Normal {
                        rng.uniform() * SQRT3
                    } else {
                        0.0
                    };
                    *v = (*v * sscale + n * nscale) * co;
                }
            }
            m += bs;
        }
    }

    // ------------------------------------------------------------ synthesis

    /// FFmpeg 9.0.2 compatibility (observed black-box, see STATUS.md): the first audio
    /// block in which the full-bandwidth channels use different transform lengths
    /// rewrites the overlap delay lines of the coded channels before the inverse
    /// transform as if a previously down-mixed delay had to be up-mixed. A/52 does not
    /// do this; it is applied only when `DecoderOptions::ffmpeg_compat` is set.
    fn ffmpeg_first_mixed_transform_quirk(&mut self) {
        let d = &mut self.delay;
        match self.acmod {
            0 | 2 => d[1] = d[0],
            4 => d[2].fill(0.0),
            6 => {
                d[3].fill(0.0);
                d[2].fill(0.0);
            }
            3 | 5 | 7 => {
                if self.acmod == 7 {
                    d[4].fill(0.0);
                }
                if self.acmod >= 5 {
                    d[3].fill(0.0);
                }
                d[2] = d[1];
                d[1].fill(0.0);
            }
            _ => {}
        }
    }

    fn synthesize(&mut self, blk: usize, imdct: &mut Imdct, sink: &mut BlockSink<'_>) {
        let chans: [usize; 6] = [0, 1, 2, 3, 4, LFE];
        for &ch in chans.iter() {
            if (ch == LFE && !self.lfeon) || (ch != LFE && ch >= self.nfchans) {
                continue;
            }
            let short = ch != LFE && self.blksw[ch];
            let out: &mut [f32] = match sink.map[ch] {
                Some(p) => {
                    let o = sink.base + blk * 256;
                    &mut sink.planes[p][o..o + 256]
                }
                None => &mut sink.scratch[..],
            };
            if short {
                imdct.short(&self.coef[ch], &mut self.delay[ch], out);
            } else {
                imdct.long(&self.coef[ch], &mut self.delay[ch], out);
            }
        }
    }
}

/// 2^-23: scale of the Q23 coefficient integers.
const Q23_SCALE: f32 = 1.0 / 8388608.0;
const SQRT3: f32 = 1.732_050_8;

static AHT_IDCT: std::sync::LazyLock<[[f32; 6]; 6]> =
    std::sync::LazyLock::new(crate::quant::aht_idct_matrix);

/// Small stack vector of coded channel indices.
struct ArrayVec6 {
    v: [usize; 6],
    n: usize,
}

impl FromIterator<usize> for ArrayVec6 {
    fn from_iter<I: IntoIterator<Item = usize>>(iter: I) -> Self {
        let mut a = ArrayVec6 { v: [0; 6], n: 0 };
        for x in iter {
            if a.n < 6 {
                a.v[a.n] = x;
                a.n += 1;
            }
        }
        a
    }
}

impl ArrayVec6 {
    fn iter(&self) -> core::slice::Iter<'_, usize> {
        self.v[..self.n].iter()
    }
}

/// Decode grouped differential exponents (7.1.3) into exps[first..].
fn decode_exponents(
    br: &mut BitReader<'_>,
    exps: &mut [u8; 256],
    absexp: u8,
    first: usize,
    ngrps: usize,
    gs: usize,
) -> Result<(), E> {
    let mut prev = absexp as i32;
    let mut pos = first;
    if pos + ngrps * 3 * gs > 256 {
        return Err(bad("exponent range"));
    }
    for _ in 0..ngrps {
        let gexp = br.get(7) as usize;
        if gexp > 124 {
            return Err(bad("grouped exponent > 124"));
        }
        let d = [gexp / 25, (gexp % 25) / 5, gexp % 5];
        for &m in d.iter() {
            prev += m as i32 - 2;
            if !(0..=24).contains(&prev) {
                return Err(bad("exponent out of range"));
            }
            let v = prev as u8;
            match gs {
                1 => exps[pos] = v,
                2 => {
                    exps[pos] = v;
                    exps[pos + 1] = v;
                }
                _ => {
                    exps[pos] = v;
                    exps[pos + 1] = v;
                    exps[pos + 2] = v;
                    exps[pos + 3] = v;
                }
            }
            pos += gs;
        }
    }
    Ok(())
}

/// Unpack mantissas for bins start..end (7.3) as Q23 integers shifted by the exponent.
#[allow(clippy::too_many_arguments)]
#[inline]
fn read_mantissas(
    br: &mut BitReader<'_>,
    bap: &[u8; 256],
    exps: &[u8; 256],
    start: usize,
    end: usize,
    dither: bool,
    rng: &mut Rng,
    t: &GroupTables,
    g: &mut Groups,
    out: &mut [i32; 256],
) {
    for bin in start..end {
        let b = bap[bin];
        let m = match b {
            0 => {
                if dither {
                    rng.dither_q23()
                } else {
                    0
                }
            }
            1 => {
                if g.n1 == 0 {
                    g.g1 = t.g1[br.get(5) as usize];
                    g.n1 = 3;
                }
                g.n1 -= 1;
                g.g1[2 - g.n1]
            }
            2 => {
                if g.n2 == 0 {
                    g.g2 = t.g2[br.get(7) as usize];
                    g.n2 = 3;
                }
                g.n2 -= 1;
                g.g2[2 - g.n2]
            }
            3 => t.b3[br.get(3) as usize],
            4 => {
                if g.n4 == 0 {
                    g.g4 = t.g4[br.get(7) as usize];
                    g.n4 = 2;
                }
                g.n4 -= 1;
                g.g4[1 - g.n4]
            }
            5 => t.b5[br.get(4) as usize],
            _ => {
                let bits = BAP_BITS[b as usize] as u32;
                br.get_signed(bits) << (24 - bits)
            }
        };
        out[bin] = m >> exps[bin];
    }
}

/// One gain-adaptive-quantized AHT mantissa (Annex E 3.4.4.2, Tables E3.5/E3.6).
#[inline]
fn gaq_mantissa(br: &mut BitReader<'_>, hebap: usize, m: u32, gain_log: u8) -> f32 {
    let idx = hebap - 8;
    match gain_log {
        0 => {
            let x = br.get_signed(m) as f32 * EXP_SCALE[(m - 1) as usize];
            x + frac16(GAQ_A1[idx] as u16) * x
        }
        1 => {
            let sb = m - 1;
            let s = br.get_signed(sb);
            if s == -(1 << (sb - 1)) {
                let lb = m - 1;
                let x = br.get_signed(lb) as f32 * EXP_SCALE[(lb - 1) as usize];
                let b = if x >= 0.0 {
                    frac16(GAQ_B2_POS)
                } else {
                    frac16(GAQ_B2_NEG[idx])
                };
                x + frac16(GAQ_A2[idx]) * x + b
            } else {
                s as f32 * EXP_SCALE[(sb - 1) as usize] * 0.5
            }
        }
        _ => {
            let sb = m - 2;
            let s = br.get_signed(sb);
            if s == -(1 << (sb - 1)) {
                let lb = m;
                let x = br.get_signed(lb) as f32 * EXP_SCALE[(lb - 1) as usize];
                let b = if x >= 0.0 {
                    frac16(GAQ_B4_POS)
                } else {
                    frac16(GAQ_B4_NEG[idx])
                };
                x + frac16(GAQ_A4[idx]) * x + b
            } else {
                s as f32 * EXP_SCALE[(sb - 1) as usize] * 0.25
            }
        }
    }
}

#[cfg(test)]
mod noise_tests {
    use super::*;

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
