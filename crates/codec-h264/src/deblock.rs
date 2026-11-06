//! Deblocking filter process (clause 8.7), applied to the whole decoded picture after all of
//! its slices are reconstructed. Supports frames, field pictures and MBAFF frames.

use crate::mbinfo::{Kind, MbInfo};
use crate::picture::Motion;
use crate::slicedec::CurPic;
use crate::tables::{ALPHA, BETA, TC0};

/// Plane view used by the deblocking filter (field pictures see one field).
struct View {
    base: usize,
    stride: usize,
}

/// p MB address and its local luma row for picture row `r` at MB column `mbx`.
#[inline]
fn mb_at_row(pic: &CurPic, mbx: usize, r: usize) -> (usize, usize) {
    if pic.mbaff {
        let py = r / 32;
        let top = 2 * (py * pic.w + mbx);
        let rr = r % 32;
        if pic.mbs[top].field {
            (top + (rr & 1), rr >> 1)
        } else {
            (top + rr / 16, rr % 16)
        }
    } else {
        ((r / 16) * pic.w + mbx, r % 16)
    }
}

/// Luma origin row and line step (dy) of an MB in picture coordinates (6.4.1).
#[inline]
fn mb_origin(pic: &CurPic, addr: usize) -> (usize, usize, usize, bool) {
    if pic.mbaff {
        let pair = addr / 2;
        let mbx = pair % pic.w;
        let py = pair / pic.w;
        if pic.mbs[addr].field {
            (mbx, py * 32 + (addr & 1), 2, true)
        } else {
            (mbx, py * 32 + (addr & 1) * 16, 1, false)
        }
    } else {
        (addr % pic.w, (addr / pic.w) * 16, 1, pic.structure != 0)
    }
}

struct BsCtx<'a> {
    mbs: &'a [MbInfo],
    motion: &'a Motion,
    /// Per-MB uniform motion summaries of the MBs deblocked so far.
    uni: &'a [Uni],
    field_pic: bool,
    mbaff: bool,
}

/// Motion of an inter MB whose 16 blocks share one reference picture and motion vector per
/// list (`rid[0] == i32::MIN`: not uniform, intra or not yet known).
#[derive(Clone, Copy)]
struct Uni {
    rid: [i32; 2],
    mv: [[i16; 2]; 2],
}

const NOT_UNI: Uni = Uni {
    rid: [i32::MIN; 2],
    mv: [[0; 2]; 2],
};

#[inline]
fn uniform_motion(m: &Motion, q: usize) -> Uni {
    let mut u = Uni {
        rid: [-1; 2],
        mv: [[0; 2]; 2],
    };
    for l in 0..2 {
        let r: &[i32; 4] = m.ref_id[l][q * 4..q * 4 + 4].try_into().unwrap();
        if r[1] != r[0] || r[2] != r[0] || r[3] != r[0] {
            return NOT_UNI;
        }
        u.rid[l] = r[0];
        if r[0] >= 0 {
            let mv: &[[i16; 2]; 16] = m.mv[l][q * 16..q * 16 + 16].try_into().unwrap();
            if mv.iter().any(|v| *v != mv[0]) {
                return NOT_UNI;
            }
            u.mv[l] = mv[0];
        }
    }
    u
}

impl BsCtx<'_> {
    /// bS for a non-mixed MB edge between blocks pb[k] of MB p and qb[k] of MB q (8.7.2.1).
    #[inline(always)]
    fn edge_bs(
        &self,
        p: usize,
        pb: [usize; 4],
        q: usize,
        qb: [usize; 4],
        vertical: bool,
    ) -> [u8; 4] {
        let mp = &self.mbs[p];
        let mq = &self.mbs[q];
        if mp.kind.is_intra() || mq.kind.is_intra() {
            let both_frame = !mp.field && !mq.field && !self.field_pic;
            let b = if both_frame || ((self.mbaff || self.field_pic) && vertical) {
                4
            } else {
                3
            };
            return [b; 4];
        }
        let m = self.motion;
        let limy = if mq.field || self.field_pic { 2 } else { 4 };
        let (np, nq) = (mp.nzmask as u32, mq.nzmask as u32);
        let (up, uq) = (self.uni[p], self.uni[q]);
        if up.rid[0] != i32::MIN && uq.rid[0] != i32::MIN {
            // Both MBs have uniform motion: one motion comparison serves the whole edge.
            let b = motion_bs(up.rid, up.mv, uq.rid, uq.mv, limy);
            let mut out = [b; 4];
            for k in 0..4 {
                if (np >> (pb[k] & 15)) & 1 != 0 || (nq >> (qb[k] & 15)) & 1 != 0 {
                    out[k] = 2;
                }
            }
            return out;
        }
        let mvp0: &[[i16; 2]; 16] = m.mv[0][p * 16..p * 16 + 16].try_into().unwrap();
        let mvp1: &[[i16; 2]; 16] = m.mv[1][p * 16..p * 16 + 16].try_into().unwrap();
        let mvq0: &[[i16; 2]; 16] = m.mv[0][q * 16..q * 16 + 16].try_into().unwrap();
        let mvq1: &[[i16; 2]; 16] = m.mv[1][q * 16..q * 16 + 16].try_into().unwrap();
        let rp0: &[i32; 4] = m.ref_id[0][p * 4..p * 4 + 4].try_into().unwrap();
        let rp1: &[i32; 4] = m.ref_id[1][p * 4..p * 4 + 4].try_into().unwrap();
        let rq0: &[i32; 4] = m.ref_id[0][q * 4..q * 4 + 4].try_into().unwrap();
        let rq1: &[i32; 4] = m.ref_id[1][q * 4..q * 4 + 4].try_into().unwrap();
        let mut out = [0u8; 4];
        for k in 0..4 {
            let (a, b) = (pb[k] & 15, qb[k] & 15);
            let (a8, b8) = ((a / 8) * 2 + (a % 4) / 2, (b / 8) * 2 + (b % 4) / 2);
            out[k] = if (np >> a) & 1 != 0 || (nq >> b) & 1 != 0 {
                2
            } else if rp0[a8] == rq0[b8]
                && rp1[a8] == rq1[b8]
                && mvp0[a] == mvq0[b]
                && mvp1[a] == mvq1[b]
            {
                0
            } else {
                motion_bs(
                    [rp0[a8], rp1[a8]],
                    [mvp0[a], mvp1[a]],
                    [rq0[b8], rq1[b8]],
                    [mvq0[b], mvq1[b]],
                    limy,
                )
            };
        }
        out
    }

    /// Boundary strength between 4x4 block pb of MB p and block qb of MB q (8.7.2.1).
    #[inline]
    #[allow(clippy::too_many_arguments)] // kernel takes explicit sample geometry instead of a wrapper struct
    fn bs(
        &self,
        p: usize,
        pb: usize,
        q: usize,
        qb: usize,
        mb_edge: bool,
        vertical: bool,
        mixed: bool,
    ) -> u8 {
        let mp = &self.mbs[p];
        let mq = &self.mbs[q];
        let ip = mp.kind.is_intra();
        let iq = mq.kind.is_intra();
        if ip || iq {
            if mb_edge {
                let both_frame = !mp.field && !mq.field && !self.field_pic;
                if both_frame || ((self.mbaff || self.field_pic) && vertical) {
                    return 4;
                }
            }
            return 3;
        }
        if (mp.nzmask >> pb) & 1 != 0 || (mq.nzmask >> qb) & 1 != 0 {
            return 2;
        }
        if mixed {
            return 1;
        }
        let m = self.motion;
        let pb8 = p * 4 + (pb / 8) * 2 + (pb % 4) / 2;
        let qb8 = q * 4 + (qb / 8) * 2 + (qb % 4) / 2;
        let rp = [m.ref_id[0][pb8], m.ref_id[1][pb8]];
        let rq = [m.ref_id[0][qb8], m.ref_id[1][qb8]];
        let mvp = [m.mv[0][p * 16 + pb], m.mv[1][p * 16 + pb]];
        let mvq = [m.mv[0][q * 16 + qb], m.mv[1][q * 16 + qb]];
        let limy = if mq.field || self.field_pic { 2 } else { 4 };
        let diff = |a: [i16; 2], b: [i16; 2]| -> bool {
            (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= limy
        };
        let np = (rp[0] >= 0) as u8 + (rp[1] >= 0) as u8;
        let nq = (rq[0] >= 0) as u8 + (rq[1] >= 0) as u8;
        if np != nq {
            return 1;
        }
        if np == 1 {
            let (r1, m1) = if rp[0] >= 0 {
                (rp[0], mvp[0])
            } else {
                (rp[1], mvp[1])
            };
            let (r2, m2) = if rq[0] >= 0 {
                (rq[0], mvq[0])
            } else {
                (rq[1], mvq[1])
            };
            if r1 != r2 {
                return 1;
            }
            return diff(m1, m2) as u8;
        }
        if np == 0 {
            return 0;
        }
        // Two motion vectors each.
        if !((rp[0] == rq[0] && rp[1] == rq[1]) || (rp[0] == rq[1] && rp[1] == rq[0])) {
            return 1;
        }
        if rp[0] != rp[1] {
            if rp[0] == rq[0] {
                (diff(mvp[0], mvq[0]) || diff(mvp[1], mvq[1])) as u8
            } else {
                (diff(mvp[0], mvq[1]) || diff(mvp[1], mvq[0])) as u8
            }
        } else {
            ((diff(mvp[0], mvq[0]) || diff(mvp[1], mvq[1]))
                && (diff(mvp[0], mvq[1]) || diff(mvp[1], mvq[0]))) as u8
        }
    }
}

/// Samples of one edge: lanes p3, p2, p1, p0, q0, q1, q2, q3 (index 0..8), up to 16 lines.
type Lanes = [[u8; 16]; 8];

/// Per-line filter parameters (bS = 0 disables the line).
#[derive(Clone, Copy)]
struct LineParams {
    bs: [u8; 16],
    alpha: [i16; 16],
    beta: [i16; 16],
    tc0: [i16; 16],
}

impl LineParams {
    /// Parameters for an edge with one qPav and bS constant per segment of `SEG` lines.
    #[inline(always)]
    fn uniform<const SEG: usize>(bs4: [u8; 4], qp_avg: i32, a_off: i32, b_off: i32) -> Self {
        let ia = (qp_avg + a_off).clamp(0, 51) as usize;
        let ib = (qp_avg + b_off).clamp(0, 51) as usize;
        let tc4 = bs4.map(|b| {
            if b > 0 && b < 4 {
                TC0[b as usize - 1][ia] as i16
            } else {
                0
            }
        });
        LineParams {
            bs: std::array::from_fn(|i| if i < 4 * SEG { bs4[i / SEG] } else { 0 }),
            alpha: [ALPHA[ia] as i16; 16],
            beta: [BETA[ib] as i16; 16],
            tc0: std::array::from_fn(|i| if i < 4 * SEG { tc4[i / SEG] } else { 0 }),
        }
    }
    fn new() -> Self {
        LineParams {
            bs: [0; 16],
            alpha: [0; 16],
            beta: [0; 16],
            tc0: [0; 16],
        }
    }
    #[inline(always)]
    fn set(&mut self, k: usize, bs: u8, qp_avg: i32, a_off: i32, b_off: i32) {
        let ia = (qp_avg + a_off).clamp(0, 51) as usize;
        let ib = (qp_avg + b_off).clamp(0, 51) as usize;
        self.bs[k] = bs;
        self.alpha[k] = ALPHA[ia] as i16;
        self.beta[k] = BETA[ib] as i16;
        self.tc0[k] = if bs > 0 && bs < 4 {
            TC0[bs as usize - 1][ia] as i16
        } else {
            0
        };
    }
}

/// Gather an edge whose lines run along a row (horizontal edge): lane k = row q0 + (k-4)*step.
#[inline(always)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn load_h<const N: usize>(s: &[u8], q0: usize, step: usize, l: &mut Lanes) {
    for k in 0..8 {
        let o = q0 + k * step - 4 * step;
        let src: &[u8; N] = s[o..o + N].try_into().unwrap();
        l[k][..N].copy_from_slice(src);
    }
}

#[inline(always)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn store_h<const N: usize>(s: &mut [u8], q0: usize, step: usize, l: &Lanes, lo: usize, hi: usize) {
    for k in lo..hi {
        let o = q0 + k * step - 4 * step;
        let dst: &mut [u8; N] = (&mut s[o..o + N]).try_into().unwrap();
        dst.copy_from_slice(&l[k][..N]);
    }
}

/// In-register transpose of an 8x8 byte matrix held as big-endian rows (column 0 = MSB).
#[inline(always)]
fn transpose8(r: &mut [u64; 8]) {
    for i in 0..4 {
        let t = (r[i] ^ (r[i + 4] >> 32)) & 0x0000_0000_FFFF_FFFF;
        r[i] ^= t;
        r[i + 4] ^= t << 32;
    }
    for i in [0, 1, 4, 5] {
        let t = (r[i] ^ (r[i + 2] >> 16)) & 0x0000_FFFF_0000_FFFF;
        r[i] ^= t;
        r[i + 2] ^= t << 16;
    }
    for i in [0, 2, 4, 6] {
        let t = (r[i] ^ (r[i + 1] >> 8)) & 0x00FF_00FF_00FF_00FF;
        r[i] ^= t;
        r[i + 1] ^= t << 8;
    }
}

/// Gather a vertical edge: line i is row q0 + i*line_step, lane k = column (k - 4).
#[inline(always)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn load_v<const N: usize>(s: &[u8], q0: usize, line_step: usize, l: &mut Lanes) {
    for half in 0..N / 8 {
        let mut r = [0u64; 8];
        for i in 0..8 {
            let o = q0 + (half * 8 + i) * line_step - 4;
            r[i] = u64::from_be_bytes(s[o..o + 8].try_into().unwrap());
        }
        transpose8(&mut r);
        for k in 0..8 {
            l[k][half * 8..half * 8 + 8].copy_from_slice(&r[k].to_be_bytes());
        }
    }
}

/// Scatter a vertical edge back (all 8 samples of each line; p3 / q3 are unchanged).
#[inline(always)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn store_v<const N: usize>(
    s: &mut [u8],
    q0: usize,
    line_step: usize,
    l: &Lanes,
    _lo: usize,
    _hi: usize,
) {
    for half in 0..N / 8 {
        let mut r = [0u64; 8];
        for k in 0..8 {
            r[k] = u64::from_be_bytes(l[k][half * 8..half * 8 + 8].try_into().unwrap());
        }
        transpose8(&mut r);
        for i in 0..8 {
            let o = q0 + (half * 8 + i) * line_step - 4;
            s[o..o + 8].copy_from_slice(&r[i].to_be_bytes());
        }
    }
}

#[inline(always)]
fn m16(c: bool) -> i16 {
    -(c as i16)
}
#[inline(always)]
fn sel16(m: i16, a: i16, b: i16) -> i16 {
    (a & m) | (b & !m)
}

/// Luma edge filter (8.7.2.3 / 8.7.2.4) on all 16 lines of `l`: straight-line integer code with
/// mask selects so the loops vectorise. Edges without bS = 4 lines use the cheaper normal filter.
#[inline(never)]
fn filter_luma(l: &mut Lanes, pr: &LineParams) {
    // filterSamplesFlag per line (8-460).
    let mut f = [0i16; 16];
    let mut any = 0i16;
    let mut any4 = 0i16;
    for i in 0..16 {
        let p1 = l[2][i] as i16;
        let p0 = l[3][i] as i16;
        let q0 = l[4][i] as i16;
        let q1 = l[5][i] as i16;
        let al = pr.alpha[i];
        let be = pr.beta[i];
        let m = m16(pr.bs[i] != 0)
            & m16((p0 - q0).abs() < al)
            & m16((p1 - p0).abs() < be)
            & m16((q1 - q0).abs() < be);
        f[i] = m;
        any |= m;
        any4 |= m & m16(pr.bs[i] == 4);
    }
    if any == 0 {
        return;
    }
    let _ = any4;
    filter_luma_full(l, pr, &f);
}

/// Luma filter for edges containing bS = 4 lines (8.7.2.3 and 8.7.2.4).
#[inline(always)]
#[allow(clippy::manual_clamp)] // Ord::clamp asserts min <= max at run time, which blocks vectorisation
fn filter_luma_full(l: &mut Lanes, pr: &LineParams, f: &[i16; 16]) {
    let mut o = [[0u8; 16]; 6];
    for i in 0..16 {
        let p3 = l[0][i] as i16;
        let p2 = l[1][i] as i16;
        let p1 = l[2][i] as i16;
        let p0 = l[3][i] as i16;
        let q0 = l[4][i] as i16;
        let q1 = l[5][i] as i16;
        let q2 = l[6][i] as i16;
        let q3 = l[7][i] as i16;
        let al = pr.alpha[i];
        let be = pr.beta[i];
        let tc0 = pr.tc0[i];
        let f = f[i];
        let is4 = m16(pr.bs[i] == 4);
        let ap = m16((p2 - p0).abs() < be);
        let aq = m16((q2 - q0).abs() < be);
        let tc = tc0 - ap - aq;
        let d = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).max(-tc).min(tc);
        let np0 = (p0 + d).clamp(0, 255);
        let nq0 = (q0 - d).clamp(0, 255);
        let avg = (p0 + q0 + 1) >> 1;
        let np1 = p1 + ((p2 + avg - (p1 << 1)) >> 1).max(-tc0).min(tc0);
        let nq1 = q1 + ((q2 + avg - (q1 << 1)) >> 1).max(-tc0).min(tc0);
        let strong = m16((p0 - q0).abs() < ((al >> 2) + 2));
        let sp = ap & strong;
        let sq = aq & strong;
        let sp0 = sel16(
            sp,
            (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3,
            (2 * p1 + p0 + q1 + 2) >> 2,
        );
        let sp1 = sel16(sp, (p2 + p1 + p0 + q0 + 2) >> 2, p1);
        let sp2 = sel16(sp, (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3, p2);
        let sq0 = sel16(
            sq,
            (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3,
            (2 * q1 + q0 + p1 + 2) >> 2,
        );
        let sq1 = sel16(sq, (p0 + q0 + q1 + q2 + 2) >> 2, q1);
        let sq2 = sel16(sq, (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3, q2);
        let fp0 = sel16(is4, sp0, np0);
        let fp1 = sel16(is4, sp1, sel16(ap, np1, p1));
        let fp2 = sel16(is4, sp2, p2);
        let fq0 = sel16(is4, sq0, nq0);
        let fq1 = sel16(is4, sq1, sel16(aq, nq1, q1));
        let fq2 = sel16(is4, sq2, q2);
        o[0][i] = sel16(f, fp2, p2) as u8;
        o[1][i] = sel16(f, fp1, p1).clamp(0, 255) as u8;
        o[2][i] = sel16(f, fp0, p0) as u8;
        o[3][i] = sel16(f, fq0, q0) as u8;
        o[4][i] = sel16(f, fq1, q1).clamp(0, 255) as u8;
        o[5][i] = sel16(f, fq2, q2) as u8;
    }
    l[1..7].copy_from_slice(&o);
}

/// Chroma edge filter (chromaStyleFilteringFlag = 1) on 8 lines.
#[inline(never)]
#[allow(clippy::manual_clamp)] // Ord::clamp asserts min <= max at run time, which blocks vectorisation
fn filter_chroma(l: &mut Lanes, pr: &LineParams, _n: usize) {
    let mut op = [0u8; 8];
    let mut oq = [0u8; 8];
    for i in 0..8 {
        let p1 = l[2][i] as i16;
        let p0 = l[3][i] as i16;
        let q0 = l[4][i] as i16;
        let q1 = l[5][i] as i16;
        let al = pr.alpha[i];
        let be = pr.beta[i];
        let bs = pr.bs[i] as i16;
        let f = m16(bs != 0)
            & m16((p0 - q0).abs() < al)
            & m16((p1 - p0).abs() < be)
            & m16((q1 - q0).abs() < be);
        let is4 = m16(bs == 4);
        let tc = pr.tc0[i] + 1;
        let d = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).max(-tc).min(tc);
        let np0 = (p0 + d).clamp(0, 255);
        let nq0 = (q0 - d).clamp(0, 255);
        let sp0 = (2 * p1 + p0 + q1 + 2) >> 2;
        let sq0 = (2 * q1 + q0 + p1 + 2) >> 2;
        op[i] = sel16(f, sel16(is4, sp0, np0), p0) as u8;
        oq[i] = sel16(f, sel16(is4, sq0, nq0), q0) as u8;
    }
    l[3][..8].copy_from_slice(&op);
    l[4][..8].copy_from_slice(&oq);
}

/// Deblocking progress of the picture being decoded. Rows of macroblocks (MB pair rows with
/// MBAFF) are filtered in order as soon as the row and the row below it are fully decoded:
/// filtering a row only touches that row and the bottom lines of the row above, and the row
/// below has then already read the unfiltered samples it predicts from. The result equals
/// filtering the whole picture after decoding (8.7), with the samples still in cache.
pub(crate) struct Progress {
    /// Macroblocks still to be decoded per row.
    left: Vec<u32>,
    /// Macroblocks per row.
    unit: usize,
    /// Next row to filter.
    next: usize,
    /// Uniform motion summaries of the macroblocks filtered so far.
    uni: Vec<Uni>,
}

impl Progress {
    pub(crate) fn new(w: usize, h: usize, mbaff: bool) -> Self {
        let (unit, rows) = if mbaff { (2 * w, h / 2) } else { (w, h) };
        Progress {
            left: vec![unit as u32; rows],
            unit,
            next: 0,
            uni: vec![NOT_UNI; w * h],
        }
    }
}

/// Note that macroblock `addr` is decoded; filter the rows that became ready.
pub(crate) fn mb_done(pic: &mut CurPic, addr: usize) {
    let p = &mut pic.dbk;
    let r = addr / p.unit;
    if r >= p.left.len() || p.left[r] == 0 {
        return;
    }
    p.left[r] -= 1;
    if p.left[r] == 0 {
        advance(pic, false);
    }
}

/// Filter every row not yet filtered (end of picture; undecoded macroblocks are skipped).
pub(crate) fn finish(pic: &mut CurPic) {
    advance(pic, true);
}

fn advance(pic: &mut CurPic, all: bool) {
    let rows = pic.dbk.left.len();
    let mut start = pic.dbk.next;
    let mut end = start;
    while end < rows
        && (all || (pic.dbk.left[end] == 0 && (end + 1 == rows || pic.dbk.left[end + 1] == 0)))
    {
        end += 1;
    }
    if end == start {
        return;
    }
    pic.dbk.next = end;
    let field_pic = pic.structure != 0;
    let (vy, vc) = if field_pic {
        let par = (pic.structure - 1) as usize;
        (
            View {
                base: par * pic.buf.width,
                stride: pic.buf.width * 2,
            },
            View {
                base: par * pic.buf.cwidth,
                stride: pic.buf.cwidth * 2,
            },
        )
    } else {
        (
            View {
                base: 0,
                stride: pic.buf.width,
            },
            View {
                base: 0,
                stride: pic.buf.cwidth,
            },
        )
    };
    let mut y = std::mem::take(&mut pic.buf.y);
    let mut cb = std::mem::take(&mut pic.buf.cb);
    let mut cr = std::mem::take(&mut pic.buf.cr);
    let mut uni = std::mem::take(&mut pic.dbk.uni);
    let unit = pic.dbk.unit;
    while start < end {
        for addr in start * unit..(start + 1) * unit {
            if pic.mbs[addr].slice == 0 {
                continue;
            }
            deblock_mb(pic, &mut uni, addr, &vy, &vc, &mut y, &mut cb, &mut cr);
        }
        start += 1;
    }
    pic.dbk.uni = uni;
    pic.buf.y = y;
    pic.buf.cb = cb;
    pic.buf.cr = cr;
}

/// bS = 1 / 0 decision from motion data (8.7.2.1, the "different reference pictures or motion
/// vectors" conditions) for two blocks that are not intra and have no coefficients.
#[inline(always)]
fn motion_bs(rp: [i32; 2], mp: [[i16; 2]; 2], rq: [i32; 2], mq: [[i16; 2]; 2], limy: i32) -> u8 {
    let diff = |a: [i16; 2], b: [i16; 2]| -> bool {
        (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= limy
    };
    let np = (rp[0] >= 0) as u8 + (rp[1] >= 0) as u8;
    let nq = (rq[0] >= 0) as u8 + (rq[1] >= 0) as u8;
    if np != nq {
        return 1;
    }
    match np {
        0 => 0,
        1 => {
            let (r1, m1) = if rp[0] >= 0 {
                (rp[0], mp[0])
            } else {
                (rp[1], mp[1])
            };
            let (r2, m2) = if rq[0] >= 0 {
                (rq[0], mq[0])
            } else {
                (rq[1], mq[1])
            };
            (r1 != r2 || diff(m1, m2)) as u8
        }
        _ => {
            if !((rp[0] == rq[0] && rp[1] == rq[1]) || (rp[0] == rq[1] && rp[1] == rq[0])) {
                return 1;
            }
            if rp[0] != rp[1] {
                if rp[0] == rq[0] {
                    (diff(mp[0], mq[0]) || diff(mp[1], mq[1])) as u8
                } else {
                    (diff(mp[0], mq[1]) || diff(mp[1], mq[0])) as u8
                }
            } else {
                ((diff(mp[0], mq[0]) || diff(mp[1], mq[1]))
                    && (diff(mp[0], mq[1]) || diff(mp[1], mq[0]))) as u8
            }
        }
    }
}

/// Internal edge strengths of a non-intra MB: [vertical edge e][row], [horizontal edge e][column].
#[inline(never)]
fn internal_bs(ctx: &BsCtx, q: usize) -> ([[u8; 4]; 4], [[u8; 4]; 4]) {
    let mq = &ctx.mbs[q];
    let nz = mq.nzmask as u32;
    let m = ctx.motion;
    let r0: [i32; 4] = m.ref_id[0][q * 4..q * 4 + 4].try_into().unwrap();
    let r1: [i32; 4] = m.ref_id[1][q * 4..q * 4 + 4].try_into().unwrap();
    let mv0: [[i16; 2]; 16] = m.mv[0][q * 16..q * 16 + 16].try_into().unwrap();
    let mv1: [[i16; 2]; 16] = m.mv[1][q * 16..q * 16 + 16].try_into().unwrap();
    let limy = if mq.field || ctx.field_pic { 2 } else { 4 };
    let b8 = |b: usize| (b / 8) * 2 + (b % 4) / 2;
    let pair = |pb: usize, qb: usize| -> u8 {
        if (nz >> pb) & 1 != 0 || (nz >> qb) & 1 != 0 {
            return 2;
        }
        let (p8, q8) = (b8(pb), b8(qb));
        motion_bs(
            [r0[p8], r1[p8]],
            [mv0[pb], mv1[pb]],
            [r0[q8], r1[q8]],
            [mv0[qb], mv1[qb]],
            limy,
        )
    };
    let mut v = [[0u8; 4]; 4];
    let mut h = [[0u8; 4]; 4];
    for e in 1..4 {
        for k in 0..4 {
            v[e][k] = pair(k * 4 + e - 1, k * 4 + e);
            h[e][k] = pair((e - 1) * 4 + k, e * 4 + k);
        }
    }
    (v, h)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::needless_range_loop)] // indexed form over parallel fixed-size arrays (mirrors the spec loops, keeps the hot loop vectorisable)
fn deblock_mb(
    pic: &CurPic,
    uni: &mut [Uni],
    addr: usize,
    vy: &View,
    vc: &View,
    ys: &mut [u8],
    cbs: &mut [u8],
    crs: &mut [u8],
) {
    let q = &pic.mbs[addr];
    let sp = pic.slices[q.slice as usize - 1];
    if sp.disable_idc == 1 {
        return;
    }
    let field_pic = pic.structure != 0;
    let q_intra = q.kind.is_intra();
    if !q_intra {
        uni[addr] = uniform_motion(&pic.motion, addr);
    }
    let ctx = BsCtx {
        mbs: &pic.mbs,
        motion: &pic.motion,
        uni,
        field_pic,
        mbaff: pic.mbaff,
    };
    let (mbx, yi, dy, qfield) = mb_origin(pic, addr);
    let chroma = pic.chroma;
    let a_off = sp.alpha_off;
    let b_off = sp.beta_off;
    let same_slice = |p: usize| pic.mbs[p].slice == q.slice;
    let pair_w = pic.w;
    let left_exists = if pic.mbaff {
        !(addr / 2).is_multiple_of(pair_w)
    } else {
        !addr.is_multiple_of(pair_w)
    };
    let top_exists = if pic.mbaff {
        let first_row = addr / 2 < pair_w;
        !(first_row && (qfield || addr & 1 == 0))
    } else {
        addr >= pair_w
    };
    let qp_q = q.qp as i32;
    let qpc_q = [q.qpc[0] as i32, q.qpc[1] as i32];
    let t8 = q.t8x8;
    // Uniform motion and no coefficients: all internal edges have bS = 0.
    let int_zero = !q_intra && q.nzmask == 0 && uni[addr].rid[0] != i32::MIN;
    let (ibs_v, ibs_h) = if q_intra || int_zero {
        ([[3u8; 4]; 4], [[3u8; 4]; 4])
    } else {
        internal_bs(&ctx, addr)
    };
    let ybase_q = vy.base + yi * vy.stride + mbx * 16;
    let lstride = vy.stride * dy;
    let cyi = yi.div_ceil(2);
    let cbase_q = vc.base + cyi * vc.stride + mbx * 8;
    let cstride = vc.stride * dy;
    let mut lanes: Lanes = [[0; 16]; 8];

    // ---------------------------------------------------------------- vertical edges
    // The left edge is "simple" (one p MB with the same row structure) except for MBAFF pairs of
    // different field/frame type.
    let left_simple = !pic.mbaff || (left_exists && pic.mbs[2 * (addr / 2 - 1)].field == qfield);
    if left_exists && left_simple {
        let p = if pic.mbaff {
            2 * (addr / 2 - 1) + (addr & 1)
        } else {
            addr - 1
        };
        if !(sp.disable_idc == 2 && !same_slice(p)) {
            let bs4 = ctx.edge_bs(p, [3, 7, 11, 15], addr, [0, 4, 8, 12], true);
            if bs4 != [0; 4] {
                let mp = &pic.mbs[p];
                let pr =
                    LineParams::uniform::<4>(bs4, (mp.qp as i32 + qp_q + 1) >> 1, a_off, b_off);
                load_v::<16>(ys, ybase_q, lstride, &mut lanes);
                filter_luma(&mut lanes, &pr);
                store_v::<16>(ys, ybase_q, lstride, &lanes, 1, 7);
                if chroma {
                    for comp in 0..2 {
                        let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                        let qpav = (mp.qpc[comp] as i32 + qpc_q[comp] + 1) >> 1;
                        let pr = LineParams::uniform::<2>(bs4, qpav, a_off, b_off);
                        load_v::<8>(s, cbase_q, cstride, &mut lanes);
                        filter_chroma(&mut lanes, &pr, 8);
                        store_v::<8>(s, cbase_q, cstride, &lanes, 3, 5);
                    }
                }
            }
        }
    } else if left_exists {
        // Mixed MBAFF left edge: per-line p MB and bS (8.7.2.1, mixedModeEdgeFlag).
        let mut filter_left = true;
        let mut bs_left = [0u8; 16];
        let mut pmb_left = [0usize; 16];
        for k in 0..16 {
            let r = yi + dy * k;
            let (p, prow) = mb_at_row(pic, mbx - 1, r);
            pmb_left[k] = p;
            if sp.disable_idc == 2 && !same_slice(p) {
                filter_left = false;
                break;
            }
            let mixed = pic.mbs[p].field != qfield;
            bs_left[k] = ctx.bs(p, (prow / 4) * 4 + 3, addr, (k / 4) * 4, true, true, mixed);
        }
        if filter_left && bs_left.iter().any(|&b| b != 0) {
            let mut pr = LineParams::new();
            for k in 0..16 {
                if bs_left[k] != 0 {
                    pr.set(
                        k,
                        bs_left[k],
                        (pic.mbs[pmb_left[k]].qp as i32 + qp_q + 1) >> 1,
                        a_off,
                        b_off,
                    );
                }
            }
            load_v::<16>(ys, ybase_q, lstride, &mut lanes);
            filter_luma(&mut lanes, &pr);
            store_v::<16>(ys, ybase_q, lstride, &lanes, 1, 7);
            if chroma {
                for comp in 0..2 {
                    let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                    let mut pr = LineParams::new();
                    for k in 0..8 {
                        // Luma line in the same field (8.7.2: luma sample (2x, 2y) of that field).
                        let lk = if !qfield {
                            4 * (k >> 1) + (k & 1)
                        } else {
                            2 * k
                        };
                        if bs_left[lk] != 0 {
                            let qpav =
                                (pic.mbs[pmb_left[lk]].qpc[comp] as i32 + qpc_q[comp] + 1) >> 1;
                            pr.set(k, bs_left[lk], qpav, a_off, b_off);
                        }
                    }
                    load_v::<8>(s, cbase_q, cstride, &mut lanes);
                    filter_chroma(&mut lanes, &pr, 8);
                    store_v::<8>(s, cbase_q, cstride, &lanes, 3, 5);
                }
            }
        }
    }
    // Internal vertical edges.
    let mut bs_int_v = [[0u8; 4]; 4];
    if !int_zero {
        for e in 1..4 {
            if t8 && e != 2 {
                continue;
            }
            for blk in 0..4 {
                bs_int_v[e][blk] = ibs_v[e][blk];
            }
            if bs_int_v[e] == [0; 4] {
                continue;
            }
            let pr = LineParams::uniform::<4>(bs_int_v[e], qp_q, a_off, b_off);
            load_v::<16>(ys, ybase_q + e * 4, lstride, &mut lanes);
            filter_luma(&mut lanes, &pr);
            store_v::<16>(ys, ybase_q + e * 4, lstride, &lanes, 1, 7);
        }
        if chroma && bs_int_v[2] != [0; 4] {
            for comp in 0..2 {
                let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                let pr = LineParams::uniform::<2>(bs_int_v[2], qpc_q[comp], a_off, b_off);
                load_v::<8>(s, cbase_q + 4, cstride, &mut lanes);
                filter_chroma(&mut lanes, &pr, 8);
                store_v::<8>(s, cbase_q + 4, cstride, &lanes, 3, 5);
            }
        }
    }

    // ---------------------------------------------------------------- horizontal edges
    if top_exists {
        let frame_over_field = pic.mbaff && addr & 1 == 0 && !qfield && addr >= 2 * pair_w && {
            let above_bot = addr - 2 * pair_w + 1;
            pic.mbs[above_bot].field
        };
        if frame_over_field {
            // Two field-mode filterings (yE = 0: top field lines, yE = 1: bottom field lines).
            for ye in 0..2usize {
                let p = addr - 2 * pair_w + ye;
                if sp.disable_idc == 2 && !same_slice(p) {
                    continue;
                }
                let mut bs = [0u8; 4];
                for bx in 0..4 {
                    bs[bx] = ctx.bs(p, 12 + bx, addr, bx, true, false, true);
                }
                if bs == [0; 4] {
                    continue;
                }
                let qpav = (pic.mbs[p].qp as i32 + qp_q + 1) >> 1;
                let pr = LineParams::uniform::<4>(bs, qpav, a_off, b_off);
                let q0 = vy.base + (yi + ye) * vy.stride + mbx * 16;
                load_h::<16>(ys, q0, 2 * vy.stride, &mut lanes);
                filter_luma(&mut lanes, &pr);
                store_h::<16>(ys, q0, 2 * vy.stride, &lanes, 1, 7);
                if chroma {
                    for comp in 0..2 {
                        let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                        let qpav = (pic.mbs[p].qpc[comp] as i32 + qpc_q[comp] + 1) >> 1;
                        let pr = LineParams::uniform::<2>(bs, qpav, a_off, b_off);
                        let q0 = vc.base + (cyi + ye) * vc.stride + mbx * 8;
                        load_h::<8>(s, q0, 2 * vc.stride, &mut lanes);
                        filter_chroma(&mut lanes, &pr, 8);
                        store_h::<8>(s, q0, 2 * vc.stride, &lanes, 3, 5);
                    }
                }
            }
        } else {
            let prow_pic = yi - dy;
            let (p, prow) = mb_at_row(pic, mbx, prow_pic);
            if !(sp.disable_idc == 2 && !same_slice(p)) {
                let mixed = pic.mbaff && pic.mbs[p].field != qfield;
                let pr0 = (prow / 4) * 4;
                let bs = if mixed {
                    let mut bs = [0u8; 4];
                    for bx in 0..4 {
                        bs[bx] = ctx.bs(p, pr0 + bx, addr, bx, true, false, mixed);
                    }
                    bs
                } else {
                    ctx.edge_bs(
                        p,
                        [pr0, pr0 + 1, pr0 + 2, pr0 + 3],
                        addr,
                        [0, 1, 2, 3],
                        false,
                    )
                };
                if bs != [0; 4] {
                    let qpav = (pic.mbs[p].qp as i32 + qp_q + 1) >> 1;
                    let pr = LineParams::uniform::<4>(bs, qpav, a_off, b_off);
                    load_h::<16>(ys, ybase_q, lstride, &mut lanes);
                    filter_luma(&mut lanes, &pr);
                    store_h::<16>(ys, ybase_q, lstride, &lanes, 1, 7);
                    if chroma {
                        for comp in 0..2 {
                            let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                            let qpav = (pic.mbs[p].qpc[comp] as i32 + qpc_q[comp] + 1) >> 1;
                            let pr = LineParams::uniform::<2>(bs, qpav, a_off, b_off);
                            load_h::<8>(s, cbase_q, cstride, &mut lanes);
                            filter_chroma(&mut lanes, &pr, 8);
                            store_h::<8>(s, cbase_q, cstride, &lanes, 3, 5);
                        }
                    }
                }
            }
        }
    }
    // Internal horizontal edges.
    if !int_zero {
        let mut bs_int_h = [[0u8; 4]; 4];
        for e in 1..4 {
            if t8 && e != 2 {
                continue;
            }
            for bx in 0..4 {
                bs_int_h[e][bx] = ibs_h[e][bx];
            }
            if bs_int_h[e] == [0; 4] {
                continue;
            }
            let pr = LineParams::uniform::<4>(bs_int_h[e], qp_q, a_off, b_off);
            let row = ybase_q + e * 4 * lstride;
            load_h::<16>(ys, row, lstride, &mut lanes);
            filter_luma(&mut lanes, &pr);
            store_h::<16>(ys, row, lstride, &lanes, 1, 7);
        }
        if chroma && bs_int_h[2] != [0; 4] {
            for comp in 0..2 {
                let s: &mut [u8] = if comp == 0 { cbs } else { crs };
                let pr = LineParams::uniform::<2>(bs_int_h[2], qpc_q[comp], a_off, b_off);
                let row = cbase_q + 4 * cstride;
                load_h::<8>(s, row, cstride, &mut lanes);
                filter_chroma(&mut lanes, &pr, 8);
                store_h::<8>(s, row, cstride, &lanes, 3, 5);
            }
        }
    }
    let _ = Kind::None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpose_roundtrip() {
        let mut s = vec![0u8; 16 * 8];
        for (i, v) in s.iter_mut().enumerate() {
            *v = (i * 7 + 3) as u8;
        }
        let mut l: Lanes = [[0; 16]; 8];
        load_v::<16>(&s, 4, 8, &mut l);
        for i in 0..16 {
            for k in 0..8 {
                assert_eq!(l[k][i], s[i * 8 + k]);
            }
        }
        let mut s2 = vec![0u8; 16 * 8];
        store_v::<16>(&mut s2, 4, 8, &l, 0, 8);
        assert_eq!(s, s2);
    }
}
