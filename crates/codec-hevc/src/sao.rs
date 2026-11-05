//! Sample adaptive offset (clause 8.7.3). Reads deblocked samples from `src` and writes the
//! modified samples into `dst` (which already holds a copy of the deblocked picture).
//!
//! Each CTB is split into an interior (all edge-offset neighbours inside the CTB) processed
//! by a branch-free loop, and a one-sample border ring where the picture / slice / tile
//! neighbour rules of 8.7.3.2 are evaluated per sample.

use crate::picture::{Frame, Pixel, Plane};
use crate::ps::{Layout, Pps, Sps};
use crate::state::{F_NOFILTER, PicState};

/// Table 8-13 hPos / vPos per SaoEoClass.
const EO_POS: [[(i32, i32); 2]; 4] = [
    [(-1, 0), (1, 0)],
    [(0, -1), (0, 1)],
    [(-1, -1), (1, 1)],
    [(1, -1), (-1, 1)],
];

#[inline(always)]
fn sgn(a: i32, b: i32) -> i32 {
    (a > b) as i32 - (a < b) as i32
}

/// Offset for raw edge index e = sgn(v-a) + sgn(v-b) in -2..=2 (8-411/8-412 remapped):
/// e=-2 -> SaoOffsetVal[1], -1 -> [2], 0 -> none, 1 -> [3], 2 -> [4].
#[inline(always)]
fn eo_offset(e: i32, t: &[i32; 5]) -> i32 {
    t[(e + 2) as usize]
}

#[inline(never)]
/// Apply SAO to CTB row `ry`. `src` holds deblocked samples of component c starting at
/// component row `src_y0[c]` (the band plus one row above and below).
#[allow(clippy::too_many_arguments)]
pub fn sao_band<P: Pixel>(
    dst: &mut Frame<P>,
    src: &Frame<P>,
    src_y0: [i32; 3],
    st: &PicState,
    sps: &Sps,
    pps: &Pps,
    layout: &Layout,
    ry: i32,
) {
    let wc = sps.pic_w_ctb as i32;
    let hc = sps.pic_h_ctb as i32;
    let any_nofilter = st.any_nofilter;
    {
        for rx in 0..wc {
            let rs = (ry * wc + rx) as usize;
            let p = &st.sao[rs];
            if p.type_idx == [0, 0, 0] {
                continue;
            }
            // Neighbour CTB usability for edge offset (slice / tile restrictions).
            let cur_slice = st.ctb_slice_addr[rs];
            let cur_info = &st.slices[st.ctb_slice_idx[rs] as usize];
            let cur_ts = layout.ctb_addr_rs_to_ts[rs];
            let cur_tile = layout.tile_id[cur_ts as usize];
            let mut ok = [[false; 3]; 3];
            ok[1][1] = true;
            for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (rx + dx, ry + dy);
                    if nx < 0 || ny < 0 || nx >= wc || ny >= hc {
                        continue;
                    }
                    let nrs = (ny * wc + nx) as usize;
                    let nts = layout.ctb_addr_rs_to_ts[nrs];
                    let mut good = true;
                    if st.ctb_slice_addr[nrs] != cur_slice {
                        let n_info = &st.slices[st.ctb_slice_idx[nrs] as usize];
                        if nts < cur_ts && !cur_info.lf_across_slices {
                            good = false;
                        }
                        if cur_ts < nts && !n_info.lf_across_slices {
                            good = false;
                        }
                    }
                    if !pps.loop_filter_across_tiles && layout.tile_id[nts as usize] != cur_tile {
                        good = false;
                    }
                    ok[(dy + 1) as usize][(dx + 1) as usize] = good;
                }
            }
            // Does this CTB contain samples excluded from in-loop filtering?
            let ctb_nofilter = any_nofilter && {
                let l2 = sps.log2_ctb as usize;
                let (x0, y0) = ((rx as usize) << l2, (ry as usize) << l2);
                let x1 = (x0 + (1 << l2)).min(sps.width as usize);
                let y1 = (y0 + (1 << l2)).min(sps.height as usize);
                let mut f = false;
                for y in (y0 >> 2)..(y1 >> 2) {
                    for x in (x0 >> 2)..(x1 >> 2) {
                        f |= st.blk[y * st.w4 + x].flags & F_NOFILTER != 0;
                    }
                }
                f
            };
            for c in 0..3 {
                let t = p.type_idx[c];
                if t == 0 {
                    continue;
                }
                let shift = if c == 0 { 0 } else { 1 };
                let ctb_w = (sps.ctb_size >> shift) as i32;
                let x0 = rx * ctb_w;
                let y0 = ry * ctb_w;
                let pw = dst.planes[c].width as i32;
                let ph = dst.planes[c].height as i32;
                let w = ctb_w.min(pw - x0);
                let h = ctb_w.min(ph - y0);
                let bd = if c == 0 {
                    sps.bit_depth_luma
                } else {
                    sps.bit_depth_chroma
                };
                let max = (1i32 << bd) - 1;
                let s = &src.planes[c];
                let d = &mut dst.planes[c];
                let ss = s.stride;
                let sy0 = src_y0[c];
                let ds = d.stride;
                let offs = p.offset[c];
                if t == 1 {
                    let band_shift = bd - 5;
                    let bp = p.band_pos[c] as i16;
                    let o: [i16; 4] = [offs[0], offs[1], offs[2], offs[3]];
                    let max16 = max as i16;
                    for y in y0..y0 + h {
                        let sr = &s.data[(y - sy0) as usize * ss + x0 as usize
                            ..(y - sy0) as usize * ss + (x0 + w) as usize];
                        let dr = &mut d.data
                            [y as usize * ds + x0 as usize..y as usize * ds + (x0 + w) as usize];
                        let done = if P::IS_U8 {
                            P::simd_sao_bo(sr, dr, p.band_pos[c], &o)
                        } else {
                            P::simd_sao_bo_hi(sr, dr, band_shift, p.band_pos[c], &o, max16)
                        };
                        for (dv, &sv) in dr[done..].iter_mut().zip(sr[done..].iter()) {
                            let v = sv.to_i32() as i16;
                            let k = ((v >> band_shift) - bp) & 31;
                            let add = (o[0] & -((k == 0) as i16))
                                + (o[1] & -((k == 1) as i16))
                                + (o[2] & -((k == 2) as i16))
                                + (o[3] & -((k == 3) as i16));
                            *dv = P::from_i32((v + add).max(0).min(max16) as i32);
                        }
                    }
                } else {
                    let [(hx0, vy0), (hx1, vy1)] = EO_POS[p.eo_class[c] as usize];
                    let tab = [
                        offs[0] as i32,
                        offs[1] as i32,
                        0,
                        offs[2] as i32,
                        offs[3] as i32,
                    ];
                    let t16: [i16; 4] = [offs[0], offs[1], offs[2], offs[3]];
                    let max16 = max as i16;
                    let cls = |v: i32, o: i32| {
                        if v < o {
                            0usize
                        } else if v >= o + ctb_w {
                            2
                        } else {
                            1
                        }
                    };
                    // Samples at x = 0 / w-1 when horizontal neighbours are used.
                    let (mx0, mx1) = if hx0 != 0 { (1, w - 1) } else { (0, w) };
                    let single = |x: i32, y: i32, d: &mut Plane<P>| {
                        let (ax, ay, bx, by) = (x + hx0, y + vy0, x + hx1, y + vy1);
                        for (nx, ny) in [(ax, ay), (bx, by)] {
                            if nx < 0 || ny < 0 || nx >= pw || ny >= ph {
                                return;
                            }
                            if !ok[cls(ny, y0)][cls(nx, x0)] {
                                return;
                            }
                        }
                        let v = s.data[(y - sy0) as usize * ss + x as usize].to_i32();
                        let a = s.data[(ay - sy0) as usize * ss + ax as usize].to_i32();
                        let b = s.data[(by - sy0) as usize * ss + bx as usize].to_i32();
                        let e = sgn(v, a) + sgn(v, b);
                        d.data[y as usize * ds + x as usize] =
                            P::from_i32((v + eo_offset(e, &tab)).max(0).min(max));
                    };
                    for y in 0..h {
                        let yy = y0 + y;
                        let (ya, yb) = (yy + vy0, yy + vy1);
                        if ya < 0 || yb < 0 || ya >= ph || yb >= ph {
                            continue;
                        }
                        let interior = mx1 > mx0 && ok[cls(ya, y0)][1] && ok[cls(yb, y0)][1];
                        if interior {
                            // The first / last column joins the vector run when both of
                            // its neighbours are usable (otherwise it stays unmodified).
                            let col_ok = |x: i32| {
                                let (ax, bx) = (x + hx0, x + hx1);
                                ax >= 0
                                    && bx >= 0
                                    && ax < pw
                                    && bx < pw
                                    && ok[cls(ya, y0)][cls(ax, x0)]
                                    && ok[cls(yb, y0)][cls(bx, x0)]
                            };
                            let xs = if hx0 != 0 && col_ok(x0) { 0 } else { mx0 };
                            let xe = if hx0 != 0 && w > 1 && col_ok(x0 + w - 1) {
                                w
                            } else {
                                mx1
                            };
                            let n = (xe - xs) as usize;
                            let xb = (x0 + xs) as usize;
                            let yu = yy as usize;
                            let cur = &s.data
                                [(yy - sy0) as usize * ss + xb..(yy - sy0) as usize * ss + xb + n];
                            let ia = (ya - sy0) as usize * ss + (xb as i32 + hx0) as usize;
                            let ib = (yb - sy0) as usize * ss + (xb as i32 + hx1) as usize;
                            let ra = &s.data[ia..ia + n];
                            let rb = &s.data[ib..ib + n];
                            let dr = &mut d.data[yu * ds + xb..yu * ds + xb + n];
                            let done = if P::IS_U8 {
                                P::simd_sao_eo(cur, ra, rb, dr, &t16)
                            } else {
                                P::simd_sao_eo_hi(cur, ra, rb, dr, &t16, max16)
                            };
                            for i in done..n {
                                let v = cur[i].to_i32() as i16;
                                let a = ra[i].to_i32() as i16;
                                let b = rb[i].to_i32() as i16;
                                let e = ((v > a) as i16 - (v < a) as i16)
                                    + ((v > b) as i16 - (v < b) as i16);
                                let add = (t16[0] & -((e == -2) as i16))
                                    + (t16[1] & -((e == -1) as i16))
                                    + (t16[2] & -((e == 1) as i16))
                                    + (t16[3] & -((e == 2) as i16));
                                dr[i] = P::from_i32((v + add).max(0).min(max16) as i32);
                            }
                        } else if hx0 != 0 {
                            single(x0, yy, d);
                            if w > 1 {
                                single(x0 + w - 1, yy, d);
                            }
                        }
                    }
                }
                if ctb_nofilter {
                    // Restore samples that must not be modified (pcm_loop_filter / bypass).
                    for y in y0..y0 + h {
                        for x in x0..x0 + w {
                            let (lx, ly) = ((x << shift) as usize, (y << shift) as usize);
                            if st.blk[(ly >> 2) * st.w4 + (lx >> 2)].flags & F_NOFILTER != 0 {
                                d.data[y as usize * ds + x as usize] =
                                    s.data[(y - sy0) as usize * ss + x as usize];
                            }
                        }
                    }
                }
            }
        }
    }
}
