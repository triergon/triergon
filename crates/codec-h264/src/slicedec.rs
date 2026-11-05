//! Slice data decoding (clause 7.3.4): macroblock loop, neighbour derivation (6.4.9 - 6.4.12)
//! and the shared per-slice / per-picture contexts.

use crate::DecodeError;
use crate::bitreader::BitReader;
use crate::cabac::Cabac;
use crate::cavlc::{self, CavlcTables};
use crate::mbinfo::*;
use crate::picture::*;
use crate::slice::{SliceHeader, SliceType};

/// Weighted prediction mode for a slice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wp {
    Default,
    Explicit,
    Implicit,
}

/// Explicit weights: [list][refIdxWP][component] = (weight, offset).
pub(crate) type WeightTable = [[[(i32, i32); 3]; 32]; 2];

/// Immutable per-slice decoding parameters.
pub(crate) struct SliceCtx {
    pub sh: SliceHeader,
    pub slice_num: u16,
    pub cabac: bool,
    pub constrained_intra: bool,
    pub t8x8_mode: bool,
    pub chroma: bool,
    pub chroma_qp_off: [i32; 2],
    /// LevelScale4x4: [iYCbCr + 3 * inter][qP % 6][raster].
    pub ls4: Box<[[[i32; 16]; 6]; 6]>,
    /// LevelScale8x8: [inter][qP % 6][raster].
    pub ls8: Box<[[[i32; 64]; 6]; 2]>,
    pub ref_list: [Vec<RefPic>; 2],
    pub direct_8x8_inference: bool,
    pub wp: Wp,
    /// Explicit weights: [list][refIdxWP] = (luma w, o), (cb w, o), (cr w, o).
    pub weights: Box<WeightTable>,
    pub log_wd: [u32; 2],
    /// Colocated picture motion for temporal/spatial direct: see `ColPic`.
    pub col: Option<ColInfo>,
}

/// Information about RefPicList1[0] for direct prediction.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColInfo {
    /// DPB index of the frame store holding RefPicList1[0].
    pub idx: usize,
    /// Parity of RefPicList1[0] (0 frame, 1 top, 2 bottom).
    pub parity: u8,
    pub short_term: bool,
}

/// The picture being decoded.
pub(crate) struct CurPic {
    pub store: usize,
    pub buf: FrameBuf,
    pub motion: Motion,
    pub mbs: Vec<MbInfo>,
    /// Width in MBs and height in MBs of this picture (field: half the frame height).
    pub w: usize,
    pub h: usize,
    /// 0 frame, 1 top field, 2 bottom field.
    pub structure: u8,
    pub mbaff: bool,
    pub poc: [i32; 2],
    pub slices: Vec<SliceDbk>,
    pub decoded_mbs: usize,
    pub chroma: bool,
    pub dbk: crate::deblock::Progress,
}

impl CurPic {
    pub(crate) fn pic_poc(&self) -> i32 {
        match self.structure {
            1 => self.poc[0],
            2 => self.poc[1],
            _ => self.poc[0].min(self.poc[1]),
        }
    }
}

/// Read-only access to reference frame stores during slice decoding.
pub(crate) struct Refs<'a> {
    pub dpb: &'a [FrameStore],
}

/// Geometry of a macroblock in the current picture's planes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MbGeom {
    pub mbx: usize,
    /// Luma y of the MB's first row in its own (field or frame) coordinates.
    pub y_pic: usize,
    pub off_y: usize,
    pub stride_y: usize,
    pub off_c: usize,
    pub stride_c: usize,
    /// The MB samples belong to a field (field picture or MBAFF field MB): parity 1 top 2 bottom.
    pub field_parity: u8,
}

/// Neighbouring macroblock addresses (6.4.9 / 6.4.10) of the current MB; None = unavailable.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NbAddr {
    pub a: Option<usize>,
    pub b: Option<usize>,
    pub c: Option<usize>,
    pub d: Option<usize>,
}

pub(crate) struct SliceDecoder<'a> {
    pub s: &'a SliceCtx,
    pub pic: &'a mut CurPic,
    pub refs: Refs<'a>,
    pub vlc: &'static CavlcTables,
    pub br: BitReader<'a>,
    pub cabac: Option<Cabac<'a>>,
    pub cur: usize,
    pub nb: NbAddr,
    pub geom: MbGeom,
    pub qp: i32,
    pub prev_qp_delta_nz: bool,
    pub mb: crate::mbdec::MbData,
    /// MBAFF: field flag decoded ahead for a skipped top MB (CAVLC) or for the pair (CABAC).
    pub pending_field: Option<bool>,
    pub last_mb_in_slice: usize,
    /// Spatial direct (refIdx, mvp, directZero) of the current MB.
    pub spatial_cache: Option<([i32; 2], [[i32; 2]; 2], bool)>,
    /// Whole-MB motion (refIdx and vector per list, refIdx -1 = list unused) when the MB is
    /// predicted as one 16x16 block, recorded while deriving it for motion compensation.
    pub mb_motion: Option<([i32; 2], [[i32; 2]; 2])>,
    /// Some reference is the first field of the current frame (second-field decoding).
    pub self_ref_possible: bool,
    /// Second-list prediction scratch for motion compensation.
    pub mc_tmp: crate::inter::McTmp,
}

impl<'a> SliceDecoder<'a> {
    pub(crate) fn new(
        s: &'a SliceCtx,
        pic: &'a mut CurPic,
        refs: Refs<'a>,
        rbsp: &'a [u8],
        rbsp_len: usize,
    ) -> SliceDecoder<'a> {
        let mut br = BitReader::new(rbsp, rbsp_len);
        br.set_pos(s.sh.data_bit_pos);
        let qp = s.sh.slice_qp;
        let store = pic.store;
        let self_ref_possible = s.ref_list.iter().flatten().any(|r| r.idx == store);
        SliceDecoder {
            s,
            pic,
            refs,
            vlc: cavlc::tables(),
            br,
            cabac: None,
            cur: 0,
            nb: NbAddr::default(),
            geom: MbGeom::default(),
            qp,
            prev_qp_delta_nz: false,
            mb: crate::mbdec::MbData::new(),
            pending_field: None,
            last_mb_in_slice: 0,
            spatial_cache: None,
            mb_motion: None,
            self_ref_possible,
            mc_tmp: crate::inter::McTmp::new(),
        }
    }

    #[inline(always)]
    pub(crate) fn slice_tag(&self) -> u16 {
        self.s.slice_num + 1
    }

    #[inline(always)]
    pub(crate) fn avail(&self, addr: usize) -> bool {
        self.pic.mbs[addr].slice == self.slice_tag()
    }

    pub(crate) fn pic_size(&self) -> usize {
        self.pic.w * self.pic.h
    }

    /// Compute neighbouring MB addresses (6.4.9 non-MBAFF, 6.4.10 MBAFF).
    pub(crate) fn compute_nb(&mut self) {
        let w = self.pic.w;
        let cur = self.cur;
        let mut nb = NbAddr::default();
        if !self.pic.mbaff {
            let x = cur % w;
            if x > 0 && self.avail(cur - 1) {
                nb.a = Some(cur - 1);
            }
            if cur >= w {
                if self.avail(cur - w) {
                    nb.b = Some(cur - w);
                }
                if x + 1 < w && self.avail(cur - w + 1) {
                    nb.c = Some(cur - w + 1);
                }
                if x > 0 && self.avail(cur - w - 1) {
                    nb.d = Some(cur - w - 1);
                }
            }
        } else {
            let pair = cur / 2;
            let x = pair % w;
            if x > 0 && self.avail(2 * (pair - 1)) {
                nb.a = Some(2 * (pair - 1));
            }
            if pair >= w {
                if self.avail(2 * (pair - w)) {
                    nb.b = Some(2 * (pair - w));
                }
                if x + 1 < w && self.avail(2 * (pair - w + 1)) {
                    nb.c = Some(2 * (pair - w + 1));
                }
                if x > 0 && self.avail(2 * (pair - w - 1)) {
                    nb.d = Some(2 * (pair - w - 1));
                }
            }
        }
        self.nb = nb;
    }

    /// Geometry of macroblock `addr` in the current picture (uses mbs[addr].field for MBAFF).
    pub(crate) fn geom_of(&self, addr: usize) -> MbGeom {
        let p = &*self.pic;
        let sy = p.buf.width;
        let sc = p.buf.cwidth;
        if p.mbaff {
            let pair = addr / 2;
            let mbx = pair % p.w;
            let py = pair / p.w;
            let bot = addr & 1;
            if p.mbs[addr].field {
                MbGeom {
                    mbx,
                    y_pic: py * 16,
                    off_y: (py * 32 + bot) * sy + mbx * 16,
                    stride_y: sy * 2,
                    off_c: (py * 16 + bot) * sc + mbx * 8,
                    stride_c: sc * 2,
                    field_parity: 1 + bot as u8,
                }
            } else {
                MbGeom {
                    mbx,
                    y_pic: py * 32 + bot * 16,
                    off_y: (py * 32 + bot * 16) * sy + mbx * 16,
                    stride_y: sy,
                    off_c: (py * 16 + bot * 8) * sc + mbx * 8,
                    stride_c: sc,
                    field_parity: 0,
                }
            }
        } else {
            let mbx = addr % p.w;
            let mby = addr / p.w;
            if p.structure == 0 {
                MbGeom {
                    mbx,
                    y_pic: mby * 16,
                    off_y: mby * 16 * sy + mbx * 16,
                    stride_y: sy,
                    off_c: mby * 8 * sc + mbx * 8,
                    stride_c: sc,
                    field_parity: 0,
                }
            } else {
                let par = (p.structure - 1) as usize;
                MbGeom {
                    mbx,
                    y_pic: mby * 16,
                    off_y: (mby * 32 + par) * sy + mbx * 16,
                    stride_y: sy * 2,
                    off_c: (mby * 16 + par) * sc + mbx * 8,
                    stride_c: sc * 2,
                    field_parity: p.structure,
                }
            }
        }
    }

    /// Non-MBAFF fast path of 6.4.12.1: the MB covering (xN, yN), or None.
    #[inline(always)]
    pub(crate) fn nb_mb_simple(&self, xn: i32, yn: i32, maxw: i32, maxh: i32) -> Option<usize> {
        let nb = &self.nb;
        if yn < 0 {
            if xn < 0 {
                nb.d
            } else if xn < maxw {
                nb.b
            } else {
                nb.c
            }
        } else if yn < maxh {
            if xn < 0 {
                nb.a
            } else if xn < maxw {
                Some(self.cur)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Clause 6.4.12: neighbouring location (xN, yN) relative to the current MB, for luma
    /// (max 16x16) or chroma (max 8x8). Returns (mbAddrN, xW, yW).
    #[inline]
    pub(crate) fn loc(&self, xn: i32, yn: i32, maxw: i32, maxh: i32) -> Option<(usize, i32, i32)> {
        let cur = self.cur;
        let nb = &self.nb;
        if !self.pic.mbaff {
            let addr = if yn < 0 {
                if xn < 0 {
                    nb.d
                } else if xn < maxw {
                    nb.b
                } else {
                    nb.c
                }
            } else if yn < maxh {
                if xn < 0 {
                    nb.a
                } else if xn < maxw {
                    Some(cur)
                } else {
                    None
                }
            } else {
                None
            }?;
            return Some((addr, (xn + maxw) % maxw, (yn + maxh) % maxh));
        }
        if yn >= maxh {
            return None;
        }
        let mbs = &self.pic.mbs;
        let cur_frame = !mbs[cur].field;
        let top = cur & 1 == 0;
        let (addr, ym): (usize, i32);
        if xn < 0 && yn < 0 {
            if cur_frame {
                if top {
                    let d = nb.d?;
                    addr = d + 1;
                    ym = yn;
                } else {
                    let a = nb.a?;
                    if !mbs[a].field {
                        addr = a;
                        ym = yn;
                    } else {
                        addr = a + 1;
                        ym = (yn + maxh) >> 1;
                    }
                }
            } else if top {
                let d = nb.d?;
                if !mbs[d].field {
                    addr = d + 1;
                    ym = 2 * yn;
                } else {
                    addr = d;
                    ym = yn;
                }
            } else {
                let d = nb.d?;
                addr = d + 1;
                ym = yn;
            }
        } else if xn < 0 {
            let a = nb.a?;
            let af = !mbs[a].field;
            if cur_frame {
                if top {
                    if af {
                        addr = a;
                        ym = yn;
                    } else if yn % 2 == 0 {
                        addr = a;
                        ym = yn >> 1;
                    } else {
                        addr = a + 1;
                        ym = yn >> 1;
                    }
                } else if af {
                    addr = a + 1;
                    ym = yn;
                } else if yn % 2 == 0 {
                    addr = a;
                    ym = (yn + maxh) >> 1;
                } else {
                    addr = a + 1;
                    ym = (yn + maxh) >> 1;
                }
            } else if top {
                if af {
                    if yn < maxh / 2 {
                        addr = a;
                        ym = yn << 1;
                    } else {
                        addr = a + 1;
                        ym = (yn << 1) - maxh;
                    }
                } else {
                    addr = a;
                    ym = yn;
                }
            } else if af {
                if yn < maxh / 2 {
                    addr = a;
                    ym = (yn << 1) + 1;
                } else {
                    addr = a + 1;
                    ym = (yn << 1) + 1 - maxh;
                }
            } else {
                addr = a + 1;
                ym = yn;
            }
        } else if xn < maxw {
            if yn >= 0 {
                return Some((cur, xn, yn));
            }
            if cur_frame {
                if top {
                    addr = nb.b? + 1;
                    ym = yn;
                } else {
                    addr = cur - 1;
                    ym = yn;
                }
            } else if top {
                let b = nb.b?;
                if !mbs[b].field {
                    addr = b + 1;
                    ym = 2 * yn;
                } else {
                    addr = b;
                    ym = yn;
                }
            } else {
                addr = nb.b? + 1;
                ym = yn;
            }
        } else {
            if yn >= 0 {
                return None;
            }
            if cur_frame {
                if top {
                    addr = nb.c? + 1;
                    ym = yn;
                } else {
                    return None;
                }
            } else if top {
                let c = nb.c?;
                if !mbs[c].field {
                    addr = c + 1;
                    ym = 2 * yn;
                } else {
                    addr = c;
                    ym = yn;
                }
            } else {
                addr = nb.c? + 1;
                ym = yn;
            }
        }
        Some((addr, (xn + maxw) % maxw, (ym + maxh) % maxh))
    }

    /// Left neighbour MB (A) for MB-level contexts (6.4.11.1): the MB covering (-1, 0).
    #[inline]
    pub(crate) fn mb_a(&self) -> Option<usize> {
        self.loc(-1, 0, 16, 16).map(|v| v.0)
    }
    /// Above neighbour MB (B) for MB-level contexts: the MB covering (0, -1).
    #[inline]
    pub(crate) fn mb_b(&self) -> Option<usize> {
        self.loc(0, -1, 16, 16).map(|v| v.0)
    }

    /// MBAFF inference of mb_field_decoding_flag when absent for the pair (7.4.4).
    fn infer_field(&self) -> bool {
        let pair = self.cur / 2;
        let w = self.pic.w;
        let x = pair % w;
        if x > 0 && self.avail(2 * (pair - 1)) {
            return self.pic.mbs[2 * (pair - 1)].field;
        }
        if pair >= w && self.avail(2 * (pair - w)) {
            return self.pic.mbs[2 * (pair - w)].field;
        }
        false
    }

    fn begin_mb(&mut self, addr: usize) -> Result<(), DecodeError> {
        if addr >= self.pic_size() {
            return Err(DecodeError::InvalidData("slice runs past picture end"));
        }
        if self.pic.mbs[addr].slice != 0 {
            return Err(DecodeError::InvalidData("macroblock decoded twice"));
        }
        self.cur = addr;
        let mut info = MbInfo {
            slice: self.slice_tag(),
            ..MbInfo::default()
        };
        if self.pic.mbaff {
            info.field = if addr & 1 == 0 {
                self.infer_field()
            } else {
                self.pic.mbs[addr - 1].field
            };
        } else {
            info.field = self.pic.structure != 0;
        }
        self.pic.mbs[addr] = info;
        self.compute_nb();
        Ok(())
    }

    fn set_field(&mut self, f: bool) {
        let addr = self.cur;
        self.pic.mbs[addr].field = f;
        if addr & 1 == 1 {
            self.pic.mbs[addr - 1].field = f;
        }
        self.compute_nb();
    }

    /// Decode all macroblocks of the slice (7.3.4 slice_data()).
    pub(crate) fn run(&mut self) -> Result<(), DecodeError> {
        let sh = &self.s.sh;
        let mbaff = self.pic.mbaff;
        let cabac = self.s.cabac;
        let mut addr = sh.first_mb as usize * (1 + mbaff as usize);
        let is_i = sh.slice_type.is_intra();
        if cabac {
            self.br.align();
            let start = self.br.pos() / 8;
            let len = self.br.len_bits() / 8;
            let init_idx = if is_i {
                0
            } else {
                sh.cabac_init_idc as usize + 1
            };
            self.cabac = Some(Cabac::new(
                self.br.data(),
                start,
                len,
                sh.slice_qp,
                init_idx,
            ));
        }
        let mut more = true;
        let mut prev_skipped = false;
        let mut bottom_skip: Option<bool> = None;
        while more {
            let mut skip_flag = false;
            if !is_i {
                if !cabac {
                    let run = self.br.ue() as usize;
                    if run > self.pic_size() {
                        return Err(DecodeError::InvalidData("mb_skip_run"));
                    }
                    prev_skipped = run > 0;
                    for i in 0..run {
                        self.pending_field = None;
                        self.begin_mb(addr)?;
                        if mbaff && addr & 1 == 0 && i + 1 == run && self.br.more_rbsp_data() {
                            // The bottom MB is coded and its mb_field_decoding_flag follows.
                            let f = self.br.flag();
                            self.pending_field = Some(f);
                            self.set_field(f);
                        }
                        self.decode_skip_mb()?;
                        self.finish_mb();
                        addr += 1;
                    }
                    if run > 0 {
                        more = self.br.more_rbsp_data();
                    }
                } else {
                    let pend = self.pending_field;
                    self.begin_mb(addr)?;
                    skip_flag = match bottom_skip.take() {
                        Some(v) if addr & 1 == 1 => v,
                        _ => self.decode_skip_flag_cabac(),
                    };
                    if skip_flag && mbaff && addr & 1 == 0 {
                        let (bs, f) = self.cabac_mbaff_lookahead();
                        bottom_skip = Some(bs);
                        self.pending_field = f;
                        if let Some(f) = f {
                            self.set_field(f);
                        }
                    } else if addr & 1 == 1 {
                        self.pending_field = pend;
                    }
                    more = !skip_flag;
                    if skip_flag {
                        self.decode_skip_mb()?;
                    }
                }
            }
            if more {
                if is_i || !cabac {
                    let pend = self.pending_field;
                    self.begin_mb(addr)?;
                    self.pending_field = pend;
                }
                if mbaff && (addr & 1 == 0 || prev_skipped) {
                    let f = match self.pending_field.take() {
                        Some(f) => f,
                        None => {
                            if cabac {
                                self.decode_field_flag_cabac()
                            } else {
                                self.br.flag()
                            }
                        }
                    };
                    self.set_field(f);
                }
                self.pending_field = None;
                self.decode_coded_mb()?;
                if !cabac {
                    self.finish_mb();
                }
            }
            if !cabac {
                more = self.br.more_rbsp_data();
                if self.br.overrun() {
                    return Err(DecodeError::InvalidData("CAVLC data overrun"));
                }
            } else {
                if !is_i {
                    prev_skipped = skip_flag;
                }
                if mbaff && addr & 1 == 0 {
                    more = true;
                } else {
                    let c = self.cabac.as_mut().unwrap();
                    more = c.terminate() == 0;
                    if c.overrun() {
                        return Err(DecodeError::InvalidData("CABAC data overrun"));
                    }
                }
                self.finish_mb();
            }
            addr += 1;
        }
        Ok(())
    }

    fn finish_mb(&mut self) {
        self.pic.decoded_mbs += 1;
        crate::deblock::mb_done(self.pic, self.cur);
        self.last_mb_in_slice = self.cur;
    }

    /// CABAC MBAFF: the top MB is skipped; decode the bottom MB's mb_skip_flag (and
    /// mb_field_decoding_flag when it is not skipped) so the pair's field mode is known.
    fn cabac_mbaff_lookahead(&mut self) -> (bool, Option<bool>) {
        let top = self.cur;
        let bot = top + 1;
        let saved_top = self.pic.mbs[top];
        self.pic.mbs[top].kind = if self.s.sh.slice_type == SliceType::B {
            Kind::BSkip
        } else {
            Kind::PSkip
        };
        self.pic.mbs[bot] = MbInfo {
            slice: self.slice_tag(),
            field: saved_top.field,
            ..MbInfo::default()
        };
        self.cur = bot;
        self.compute_nb();
        let bot_skip = self.decode_skip_flag_cabac();
        let f = if !bot_skip {
            Some(self.decode_field_flag_cabac())
        } else {
            None
        };
        self.pic.mbs[bot] = MbInfo::default();
        self.pic.mbs[top] = saved_top;
        self.cur = top;
        self.compute_nb();
        (bot_skip, f)
    }
}
