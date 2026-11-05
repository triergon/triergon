//! Intra prediction (clause 8.3) and inverse transforms / scaling (clause 8.5) kernels.

#[inline(always)]
pub(crate) fn clip1(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ---------------------------------------------------------------- transforms

#[inline(always)]
fn tr4(d: &[[i32; 4]; 4]) -> [[i32; 4]; 4] {
    let mut o = [[0i32; 4]; 4];
    for j in 0..4 {
        let e0 = d[0][j] + d[2][j];
        let e1 = d[0][j] - d[2][j];
        let e2 = (d[1][j] >> 1) - d[3][j];
        let e3 = d[1][j] + (d[3][j] >> 1);
        o[0][j] = e0 + e3;
        o[1][j] = e1 + e2;
        o[2][j] = e1 - e2;
        o[3][j] = e0 - e3;
    }
    o
}

/// 4x4 inverse transform (8.5.12.2) of `d` (raster, already scaled) added to `dst`. Both 1-D
/// passes run lane-parallel (four rows, then four columns at once).
#[inline]
pub(crate) fn idct4_add(d: &[i32; 16], dst: &mut [u8], off: usize, stride: usize) {
    // t[k][i] = d[i][k]: column k of all rows as one lane vector.
    let mut t = [[0i32; 4]; 4];
    for i in 0..4 {
        for k in 0..4 {
            t[k][i] = d[i * 4 + k];
        }
    }
    let f = tr4(&t); // f[k][i]: row i, column k
    let mut u = [[0i32; 4]; 4];
    for k in 0..4 {
        for i in 0..4 {
            u[i][k] = f[k][i];
        }
    }
    let h = tr4(&u); // h[i][j]
    for i in 0..4 {
        let row: &mut [u8; 4] = (&mut dst[off + i * stride..off + i * stride + 4])
            .try_into()
            .unwrap();
        for j in 0..4 {
            row[j] = (row[j] as i32 + ((h[i][j] + 32) >> 6)).clamp(0, 255) as u8;
        }
    }
}

/// Add a constant residual (DC-only block) of size `n`x`n`.
#[inline]
pub(crate) fn dc_add(dc: i32, n: usize, dst: &mut [u8], off: usize, stride: usize) {
    let v = (dc + 32) >> 6;
    if v == 0 {
        return;
    }
    for y in 0..n {
        let row = &mut dst[off + y * stride..off + y * stride + n];
        for p in row.iter_mut() {
            *p = (*p as i32 + v).clamp(0, 255) as u8;
        }
    }
}

/// One 8-point 1-D inverse transform (8.5.13.2) applied lane-wise.
#[inline(always)]
fn tr8<const L: usize>(d: [&[i32; L]; 8], out: &mut [[i32; L]; 8]) {
    for j in 0..L {
        let e0 = d[0][j] + d[4][j];
        let e1 = -d[3][j] + d[5][j] - d[7][j] - (d[7][j] >> 1);
        let e2 = d[0][j] - d[4][j];
        let e3 = d[1][j] + d[7][j] - d[3][j] - (d[3][j] >> 1);
        let e4 = (d[2][j] >> 1) - d[6][j];
        let e5 = -d[1][j] + d[7][j] + d[5][j] + (d[5][j] >> 1);
        let e6 = d[2][j] + (d[6][j] >> 1);
        let e7 = d[3][j] + d[5][j] + d[1][j] + (d[1][j] >> 1);
        let f0 = e0 + e6;
        let f1 = e1 + (e7 >> 2);
        let f2 = e2 + e4;
        let f3 = e3 + (e5 >> 2);
        let f4 = e2 - e4;
        let f5 = (e3 >> 2) - e5;
        let f6 = e0 - e6;
        let f7 = e7 - (e1 >> 2);
        out[0][j] = f0 + f7;
        out[1][j] = f2 + f5;
        out[2][j] = f4 + f3;
        out[3][j] = f6 + f1;
        out[4][j] = f6 - f1;
        out[5][j] = f4 - f3;
        out[6][j] = f2 - f5;
        out[7][j] = f0 - f7;
    }
}

/// 8x8 inverse transform (8.5.13.2) added to `dst`.
#[inline]
pub(crate) fn idct8_add(d: &[i32; 64], dst: &mut [u8], off: usize, stride: usize) {
    // Transpose so the row transforms run lane-parallel over the 8 rows.
    let mut t = [[0i32; 8]; 8];
    for i in 0..8 {
        for k in 0..8 {
            t[k][i] = d[i * 8 + k];
        }
    }
    let mut rt = [[0i32; 8]; 8]; // rt[k][i]: row i, output column k
    tr8::<8>(
        [&t[0], &t[1], &t[2], &t[3], &t[4], &t[5], &t[6], &t[7]],
        &mut rt,
    );
    // Transpose back: rows of the intermediate as lanes for the column transforms.
    let mut f = [[0i32; 8]; 8]; // f[i][j]
    for k in 0..8 {
        for i in 0..8 {
            f[i][k] = rt[k][i];
        }
    }
    let mut h = [[0i32; 8]; 8];
    tr8::<8>(
        [&f[0], &f[1], &f[2], &f[3], &f[4], &f[5], &f[6], &f[7]],
        &mut h,
    );
    for i in 0..8 {
        let row = &mut dst[off + i * stride..off + i * stride + 8];
        for j in 0..8 {
            row[j] = (row[j] as i32 + ((h[i][j] + 32) >> 6)).clamp(0, 255) as u8;
        }
    }
}

/// Inverse Hadamard of the Intra16x16 DC (8.5.10) and scaling. `c` raster 4x4, output in place.
pub(crate) fn luma_dc_dequant(c: &mut [i32; 16], qp: i32, ls00: i32) {
    let mut f = [0i32; 16];
    for i in 0..4 {
        let r = [c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]];
        let a = r[0] + r[1];
        let b = r[0] - r[1];
        let cc = r[2] + r[3];
        let dd = r[2] - r[3];
        // Equation 8-320 matrix rows: (1,1,1,1), (1,1,-1,-1), (1,-1,-1,1), (1,-1,1,-1).
        f[i * 4] = a + cc;
        f[i * 4 + 1] = a - cc;
        f[i * 4 + 2] = b - dd;
        f[i * 4 + 3] = b + dd;
    }
    for j in 0..4 {
        let r = [f[j], f[4 + j], f[8 + j], f[12 + j]];
        let a = r[0] + r[1];
        let b = r[0] - r[1];
        let cc = r[2] + r[3];
        let dd = r[2] - r[3];
        let o = [a + cc, a - cc, b - dd, b + dd];
        for i in 0..4 {
            let v = o[i];
            c[i * 4 + j] = if qp >= 36 {
                (v * ls00) << (qp / 6 - 6)
            } else {
                (v * ls00 + (1 << (5 - qp / 6))) >> (6 - qp / 6)
            };
        }
    }
}

/// Chroma DC 2x2 (4:2:0) transform and scaling (8.5.11), in place.
pub(crate) fn chroma_dc_dequant(c: &mut [i32; 4], qp: i32, ls00: i32) {
    let f0 = c[0] + c[1] + c[2] + c[3];
    let f1 = c[0] - c[1] + c[2] - c[3];
    let f2 = c[0] + c[1] - c[2] - c[3];
    let f3 = c[0] - c[1] - c[2] + c[3];
    let s = |f: i32| ((f * ls00) << (qp / 6)) >> 5;
    c[0] = s(f0);
    c[1] = s(f1);
    c[2] = s(f2);
    c[3] = s(f3);
}

// ---------------------------------------------------------------- intra 4x4

/// Neighbour samples for a 4x4 / 8x8 block: `top[0..n*2]` = p[x,-1], `left[0..n]` = p[-1,y],
/// `tl` = p[-1,-1], with availability flags. Top-right substitution is done by the caller.
pub(crate) struct Edge<const N: usize, const N2: usize> {
    pub top: [u8; N2],
    pub left: [u8; N],
    pub tl: u8,
    pub has_top: bool,
    pub has_left: bool,
    pub has_tl: bool,
}

#[inline(always)]
fn avg2(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}
#[inline(always)]
fn avg3(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}

/// Intra_4x4 prediction (8.3.1.2) written to dst.
pub(crate) fn pred4x4(mode: u8, e: &Edge<4, 8>, dst: &mut [u8], off: usize, stride: usize) {
    let t = |x: i32| -> i32 {
        if x < 0 {
            e.tl as i32
        } else {
            e.top[x as usize] as i32
        }
    };
    let l = |y: i32| -> i32 {
        if y < 0 {
            e.tl as i32
        } else {
            e.left[y as usize] as i32
        }
    };
    let mut p = [0u8; 16];
    match mode {
        0 => {
            for y in 0..4 {
                p[y * 4..y * 4 + 4].copy_from_slice(&e.top[0..4]);
            }
        }
        1 => {
            for y in 0..4 {
                p[y * 4..y * 4 + 4].fill(e.left[y]);
            }
        }
        2 => {
            let st: i32 = e.top[0..4].iter().map(|&v| v as i32).sum();
            let sl: i32 = e.left.iter().map(|&v| v as i32).sum();
            let v = if e.has_top && e.has_left {
                (st + sl + 4) >> 3
            } else if e.has_left {
                (sl + 2) >> 2
            } else if e.has_top {
                (st + 2) >> 2
            } else {
                128
            };
            p.fill(v as u8);
        }
        3 => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let v = if x == 3 && y == 3 {
                        (t(6) + 3 * t(7) + 2) >> 2
                    } else {
                        avg3(t(x + y), t(x + y + 1), t(x + y + 2))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
        4 => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let v = if x > y {
                        avg3(t(x - y - 2), t(x - y - 1), t(x - y))
                    } else if x < y {
                        avg3(l(y - x - 2), l(y - x - 1), l(y - x))
                    } else {
                        avg3(t(0), e.tl as i32, l(0))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
        5 => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = 2 * x - y;
                    let v = if z >= 0 && z & 1 == 0 {
                        avg2(t(x - (y >> 1) - 1), t(x - (y >> 1)))
                    } else if z >= 0 {
                        avg3(t(x - (y >> 1) - 2), t(x - (y >> 1) - 1), t(x - (y >> 1)))
                    } else if z == -1 {
                        avg3(l(0), e.tl as i32, t(0))
                    } else {
                        avg3(l(y - 1), l(y - 2), l(y - 3))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
        6 => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = 2 * y - x;
                    let v = if z >= 0 && z & 1 == 0 {
                        avg2(l(y - (x >> 1) - 1), l(y - (x >> 1)))
                    } else if z >= 0 {
                        avg3(l(y - (x >> 1) - 2), l(y - (x >> 1) - 1), l(y - (x >> 1)))
                    } else if z == -1 {
                        avg3(l(0), e.tl as i32, t(0))
                    } else {
                        avg3(t(x - 1), t(x - 2), t(x - 3))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
        7 => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let i = x + (y >> 1);
                    let v = if y & 1 == 0 {
                        avg2(t(i), t(i + 1))
                    } else {
                        avg3(t(i), t(i + 1), t(i + 2))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
        _ => {
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let z = x + 2 * y;
                    let i = y + (x >> 1);
                    let v = if z > 5 {
                        l(3)
                    } else if z == 5 {
                        (l(2) + 3 * l(3) + 2) >> 2
                    } else if z & 1 == 0 {
                        avg2(l(i), l(i + 1))
                    } else {
                        avg3(l(i), l(i + 1), l(i + 2))
                    };
                    p[(y * 4 + x) as usize] = v as u8;
                }
            }
        }
    }
    for y in 0..4 {
        dst[off + y * stride..off + y * stride + 4].copy_from_slice(&p[y * 4..y * 4 + 4]);
    }
}

// ---------------------------------------------------------------- intra 8x8

/// Intra_8x8 prediction with reference sample filtering (8.3.2.2). `e.top` holds 16 samples
/// (top-right already substituted when unavailable).
pub(crate) fn pred8x8(mode: u8, e: &Edge<8, 16>, dst: &mut [u8], off: usize, stride: usize) {
    // 8.3.2.2.1 reference sample filtering.
    let mut tp = [0i32; 16];
    let mut lp = [0i32; 8];
    let mut tlp = e.tl as i32;
    let t: [i32; 16] = std::array::from_fn(|i| e.top[i] as i32);
    let l: [i32; 8] = std::array::from_fn(|i| e.left[i] as i32);
    let tl = e.tl as i32;
    if e.has_top {
        tp[0] = if e.has_tl {
            avg3(tl, t[0], t[1])
        } else {
            (3 * t[0] + t[1] + 2) >> 2
        };
        for x in 1..15 {
            tp[x] = avg3(t[x - 1], t[x], t[x + 1]);
        }
        tp[15] = (t[14] + 3 * t[15] + 2) >> 2;
    }
    if e.has_tl {
        tlp = if !e.has_top && e.has_left {
            (3 * tl + l[0] + 2) >> 2
        } else if e.has_top && !e.has_left {
            (3 * tl + t[0] + 2) >> 2
        } else if e.has_top && e.has_left {
            avg3(t[0], tl, l[0])
        } else {
            tl
        };
    }
    if e.has_left {
        lp[0] = if e.has_tl {
            avg3(tl, l[0], l[1])
        } else {
            (3 * l[0] + l[1] + 2) >> 2
        };
        for y in 1..7 {
            lp[y] = avg3(l[y - 1], l[y], l[y + 1]);
        }
        lp[7] = (l[6] + 3 * l[7] + 2) >> 2;
    }
    let tt = |x: i32| -> i32 { if x < 0 { tlp } else { tp[x as usize] } };
    let ll = |y: i32| -> i32 { if y < 0 { tlp } else { lp[y as usize] } };
    let mut p = [0u8; 64];
    match mode {
        0 => {
            for y in 0..8 {
                for x in 0..8 {
                    p[y * 8 + x] = tp[x] as u8;
                }
            }
        }
        1 => {
            for y in 0..8 {
                p[y * 8..y * 8 + 8].fill(lp[y] as u8);
            }
        }
        2 => {
            let st: i32 = tp[0..8].iter().sum();
            let sl: i32 = lp.iter().sum();
            let v = if e.has_top && e.has_left {
                (st + sl + 8) >> 4
            } else if e.has_left {
                (sl + 4) >> 3
            } else if e.has_top {
                (st + 4) >> 3
            } else {
                128
            };
            p.fill(v as u8);
        }
        3 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let v = if x == 7 && y == 7 {
                        (tt(14) + 3 * tt(15) + 2) >> 2
                    } else {
                        avg3(tt(x + y), tt(x + y + 1), tt(x + y + 2))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
        4 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let v = if x > y {
                        avg3(tt(x - y - 2), tt(x - y - 1), tt(x - y))
                    } else if x < y {
                        avg3(ll(y - x - 2), ll(y - x - 1), ll(y - x))
                    } else {
                        avg3(tt(0), tlp, ll(0))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
        5 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = 2 * x - y;
                    let v = if z >= 0 && z & 1 == 0 {
                        avg2(tt(x - (y >> 1) - 1), tt(x - (y >> 1)))
                    } else if z >= 0 {
                        avg3(tt(x - (y >> 1) - 2), tt(x - (y >> 1) - 1), tt(x - (y >> 1)))
                    } else if z == -1 {
                        avg3(ll(0), tlp, tt(0))
                    } else {
                        avg3(ll(y - 2 * x - 1), ll(y - 2 * x - 2), ll(y - 2 * x - 3))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
        6 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = 2 * y - x;
                    let v = if z >= 0 && z & 1 == 0 {
                        avg2(ll(y - (x >> 1) - 1), ll(y - (x >> 1)))
                    } else if z >= 0 {
                        avg3(ll(y - (x >> 1) - 2), ll(y - (x >> 1) - 1), ll(y - (x >> 1)))
                    } else if z == -1 {
                        avg3(ll(0), tlp, tt(0))
                    } else {
                        avg3(tt(x - 2 * y - 1), tt(x - 2 * y - 2), tt(x - 2 * y - 3))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
        7 => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let i = x + (y >> 1);
                    let v = if y & 1 == 0 {
                        avg2(tt(i), tt(i + 1))
                    } else {
                        avg3(tt(i), tt(i + 1), tt(i + 2))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
        _ => {
            for y in 0..8i32 {
                for x in 0..8i32 {
                    let z = x + 2 * y;
                    let i = y + (x >> 1);
                    let v = if z > 13 {
                        ll(7)
                    } else if z == 13 {
                        (ll(6) + 3 * ll(7) + 2) >> 2
                    } else if z & 1 == 0 {
                        avg2(ll(i), ll(i + 1))
                    } else {
                        avg3(ll(i), ll(i + 1), ll(i + 2))
                    };
                    p[(y * 8 + x) as usize] = v as u8;
                }
            }
        }
    }
    for y in 0..8 {
        dst[off + y * stride..off + y * stride + 8].copy_from_slice(&p[y * 8..y * 8 + 8]);
    }
}

// ---------------------------------------------------------------- intra 16x16 / chroma

/// Intra_16x16 prediction (8.3.3). `top` = p[0..15,-1], `left` = p[-1,0..15].
#[allow(clippy::too_many_arguments)] // kernel takes explicit sample geometry instead of a wrapper struct
pub(crate) fn pred16x16(
    mode: u8,
    top: &[u8; 16],
    left: &[u8; 16],
    tl: u8,
    has_top: bool,
    has_left: bool,
    dst: &mut [u8],
    off: usize,
    stride: usize,
) {
    match mode {
        0 => {
            for y in 0..16 {
                dst[off + y * stride..off + y * stride + 16].copy_from_slice(top);
            }
        }
        1 => {
            for y in 0..16 {
                dst[off + y * stride..off + y * stride + 16].fill(left[y]);
            }
        }
        2 => {
            let st: i32 = top.iter().map(|&v| v as i32).sum();
            let sl: i32 = left.iter().map(|&v| v as i32).sum();
            let v = if has_top && has_left {
                (st + sl + 16) >> 5
            } else if has_left {
                (sl + 8) >> 4
            } else if has_top {
                (st + 8) >> 4
            } else {
                128
            };
            for y in 0..16 {
                dst[off + y * stride..off + y * stride + 16].fill(v as u8);
            }
        }
        _ => {
            let t = |x: i32| {
                if x < 0 {
                    tl as i32
                } else {
                    top[x as usize] as i32
                }
            };
            let l = |y: i32| {
                if y < 0 {
                    tl as i32
                } else {
                    left[y as usize] as i32
                }
            };
            let mut h = 0;
            let mut v = 0;
            for k in 0..8i32 {
                h += (k + 1) * (t(8 + k) - t(6 - k));
                v += (k + 1) * (l(8 + k) - l(6 - k));
            }
            let a = 16 * (l(15) + t(15));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for y in 0..16i32 {
                let row = off + y as usize * stride;
                let base = a + c * (y - 7) + 16;
                for x in 0..16i32 {
                    dst[row + x as usize] = clip1((base + b * (x - 7)) >> 5);
                }
            }
        }
    }
}

/// Intra chroma prediction for one 8x8 (4:2:0) component (8.3.4).
#[allow(clippy::too_many_arguments)] // kernel takes explicit sample geometry instead of a wrapper struct
pub(crate) fn pred_chroma8(
    mode: u8,
    top: &[u8; 8],
    left: &[u8; 8],
    tl: u8,
    has_top: bool,
    has_left_half: [bool; 2],
    dst: &mut [u8],
    off: usize,
    stride: usize,
) {
    match mode {
        0 => {
            for by in 0..2 {
                // Availability of p[-1, y] is checked per 4x4 chroma block (8.3.4.1 - 8.3.4.3).
                let has_left = has_left_half[by];
                for bx in 0..2 {
                    let st: i32 = top[bx * 4..bx * 4 + 4].iter().map(|&v| v as i32).sum();
                    let sl: i32 = left[by * 4..by * 4 + 4].iter().map(|&v| v as i32).sum();
                    let v = if bx == by {
                        if has_top && has_left {
                            (st + sl + 4) >> 3
                        } else if has_left {
                            (sl + 2) >> 2
                        } else if has_top {
                            (st + 2) >> 2
                        } else {
                            128
                        }
                    } else if bx > 0 {
                        // xO > 0, yO == 0
                        if has_top {
                            (st + 2) >> 2
                        } else if has_left {
                            (sl + 2) >> 2
                        } else {
                            128
                        }
                    } else if has_left {
                        (sl + 2) >> 2
                    } else if has_top {
                        (st + 2) >> 2
                    } else {
                        128
                    };
                    for y in 0..4 {
                        let o = off + (by * 4 + y) * stride + bx * 4;
                        dst[o..o + 4].fill(v as u8);
                    }
                }
            }
        }
        1 => {
            for y in 0..8 {
                dst[off + y * stride..off + y * stride + 8].fill(left[y]);
            }
        }
        2 => {
            for y in 0..8 {
                dst[off + y * stride..off + y * stride + 8].copy_from_slice(top);
            }
        }
        _ => {
            let t = |x: i32| {
                if x < 0 {
                    tl as i32
                } else {
                    top[x as usize] as i32
                }
            };
            let l = |y: i32| {
                if y < 0 {
                    tl as i32
                } else {
                    left[y as usize] as i32
                }
            };
            let mut h = 0;
            let mut v = 0;
            for k in 0..4i32 {
                h += (k + 1) * (t(4 + k) - t(2 - k));
                v += (k + 1) * (l(4 + k) - l(2 - k));
            }
            let a = 16 * (l(7) + t(7));
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;
            for y in 0..8i32 {
                let row = off + y as usize * stride;
                for x in 0..8i32 {
                    dst[row + x as usize] = clip1((a + b * (x - 3) + c * (y - 3) + 16) >> 5);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idct_dc_matches_constant() {
        let mut d = [0i32; 16];
        d[0] = 640;
        let mut a = vec![100u8; 16];
        idct4_add(&d, &mut a, 0, 4);
        let mut b = vec![100u8; 16];
        dc_add(640, 4, &mut b, 0, 4);
        assert_eq!(a, b);
        let mut d8 = [0i32; 64];
        d8[0] = -1000;
        let mut a = vec![100u8; 64];
        idct8_add(&d8, &mut a, 0, 8);
        let mut b = vec![100u8; 64];
        dc_add(-1000, 8, &mut b, 0, 8);
        assert_eq!(a, b);
    }
}
