//! Intra sample prediction (clause 8.4.4.2).
//!
//! Reference samples are held in one array `r` of length 4N+1 indexed as
//! r[2N-1-y] = p[-1][y] (y = 0..2N-1), r[2N] = p[-1][-1], r[2N+1+x] = p[x][-1].
//! This order matches the substitution search order of clause 8.4.4.2.2.

use crate::picture::{Pixel, Plane};
use crate::tables::{INTRA_PRED_ANGLE, INV_ANGLE};

pub const MAX_REF: usize = 4 * 32 + 1;

/// Reference sample substitution (8.4.4.2.2). `avail[i]` marks availability per sample.
#[inline(never)]
pub fn substitute(r: &mut [i32; MAX_REF], avail: &[bool; MAX_REF], n: usize, bit_depth: u32) {
    let len = 4 * n + 1;
    let first = avail[..len].iter().position(|&a| a);
    match first {
        None => {
            let v = 1 << (bit_depth - 1);
            r[..len].fill(v);
        }
        Some(f) => {
            if f > 0 {
                let v = r[f];
                r[..f].fill(v);
            }
            for i in f + 1..len {
                if !avail[i] {
                    r[i] = r[i - 1];
                }
            }
        }
    }
}

/// Filtering of neighbouring samples (8.4.4.2.3), luma only (ChromaArrayType 1).
#[inline(never)]
pub fn filter(r: &mut [i32; MAX_REF], n: usize, mode: u32, strong_enabled: bool, bit_depth: u32) {
    if mode == 1 || n == 4 {
        return;
    }
    let m = mode as i32;
    let min_dist = (m - 26).abs().min((m - 10).abs());
    let thres = match n {
        8 => 7,
        16 => 1,
        _ => 0,
    };
    if min_dist <= thres {
        return;
    }
    let len = 4 * n + 1;
    let c = 2 * n; // corner index
    if strong_enabled && n == 32 {
        let corner = r[c];
        let bottom = r[0]; // p[-1][63]
        let top_right = r[len - 1]; // p[63][-1]
        let lim = 1 << (bit_depth - 5);
        // p[-1][31] is r[2N-1-31] = r[32]; p[31][-1] is r[2N+1+31] = r[96].
        if (corner + top_right - 2 * r[c + 1 + 31]).abs() < lim
            && (corner + bottom - 2 * r[c - 1 - 31]).abs() < lim
        {
            // 8-37: pF[-1][y] for y = 0..62 ; 8-39: pF[x][-1] for x = 0..62.
            for y in 0..63 {
                r[c - 1 - y] = ((63 - y as i32) * corner + (y as i32 + 1) * bottom + 32) >> 6;
            }
            for x in 0..63 {
                r[c + 1 + x] = ((63 - x as i32) * corner + (x as i32 + 1) * top_right + 32) >> 6;
            }
            return;
        }
    }
    let mut out = [0i32; MAX_REF];
    out[0] = r[0];
    out[len - 1] = r[len - 1];
    for i in 1..len - 1 {
        out[i] = (r[i - 1] + 2 * r[i] + r[i + 1] + 2) >> 2;
    }
    r[..len].copy_from_slice(&out[..len]);
}

#[inline(always)]
fn left(r: &[i32; MAX_REF], n: usize, y: usize) -> i32 {
    r[2 * n - 1 - y]
}
#[inline(always)]
fn top(r: &[i32; MAX_REF], n: usize, x: usize) -> i32 {
    r[2 * n + 1 + x]
}

/// Predict an NxN block into `plane` at (x0, y0).
#[inline(never)]
pub fn predict<P: Pixel>(
    r: &[i32; MAX_REF],
    n: usize,
    mode: u32,
    is_luma: bool,
    bit_depth: u32,
    plane: &mut Plane<P>,
    (x0, y0): (usize, usize),
) {
    let stride = plane.stride;
    let base = y0 * stride + x0;
    let max = (1i32 << bit_depth) - 1;
    let corner = r[2 * n];
    match mode {
        0 => {
            // Planar (8-46)
            let shift = n.trailing_zeros() + 1;
            let tr = top(r, n, n);
            let bl = left(r, n, n);
            for y in 0..n {
                let row = &mut plane.data[base + y * stride..base + y * stride + n];
                let l = left(r, n, y);
                for (x, out) in row.iter_mut().enumerate() {
                    let v = ((n - 1 - x) as i32 * l
                        + (x + 1) as i32 * tr
                        + (n - 1 - y) as i32 * top(r, n, x)
                        + (y + 1) as i32 * bl
                        + n as i32)
                        >> shift;
                    *out = P::from_i32(v);
                }
            }
        }
        1 => {
            // DC (8-47 .. 8-52)
            let k = n.trailing_zeros();
            let mut sum = n as i32;
            for i in 0..n {
                sum += top(r, n, i) + left(r, n, i);
            }
            let dc = sum >> (k + 1);
            let dcp = P::from_i32(dc);
            for y in 0..n {
                plane.data[base + y * stride..base + y * stride + n].fill(dcp);
            }
            if is_luma && n < 32 {
                plane.data[base] = P::from_i32((left(r, n, 0) + 2 * dc + top(r, n, 0) + 2) >> 2);
                for x in 1..n {
                    plane.data[base + x] = P::from_i32((top(r, n, x) + 3 * dc + 2) >> 2);
                }
                for y in 1..n {
                    plane.data[base + y * stride] = P::from_i32((left(r, n, y) + 3 * dc + 2) >> 2);
                }
            }
        }
        _ => {
            let angle = INTRA_PRED_ANGLE[mode as usize] as i32;
            // refm[N + i] = ref[i] for i in -N..=2N.
            let mut refm = [0i32; 3 * 32 + 1];
            let off = n;
            let vertical = mode >= 18;
            // main / side accessors
            let main = |i: usize| {
                if vertical {
                    top(r, n, i)
                } else {
                    left(r, n, i)
                }
            };
            let side = |i: usize| {
                if vertical {
                    left(r, n, i)
                } else {
                    top(r, n, i)
                }
            };
            refm[off] = corner;
            for x in 1..=n {
                refm[off + x] = main(x - 1);
            }
            if angle < 0 {
                let last = (n as i32 * angle) >> 5;
                if last < -1 {
                    let inv = INV_ANGLE[(mode - 11) as usize] as i32;
                    let mut x = -1;
                    while x >= last {
                        let idx = -1 + ((x * inv + 128) >> 8);
                        // p[-1][idx] for vertical / p[idx][-1] for horizontal; idx >= 0 here.
                        refm[(off as i32 + x) as usize] = side(idx as usize);
                        x -= 1;
                    }
                }
            } else {
                for x in n + 1..=2 * n {
                    refm[off + x] = main(x - 1);
                }
            }
            if vertical {
                for y in 0..n {
                    let pos = (y as i32 + 1) * angle;
                    let idx = pos >> 5;
                    let fact = pos & 31;
                    let row = &mut plane.data[base + y * stride..base + y * stride + n];
                    let b = (off as i32 + idx + 1) as usize;
                    if fact != 0 {
                        for x in 0..n {
                            row[x] = P::from_i32(
                                ((32 - fact) * refm[b + x] + fact * refm[b + x + 1] + 16) >> 5,
                            );
                        }
                    } else {
                        for x in 0..n {
                            row[x] = P::from_i32(refm[b + x]);
                        }
                    }
                }
                if mode == 26 && is_luma && n < 32 {
                    for y in 0..n {
                        let v = top(r, n, 0) + ((left(r, n, y) - corner) >> 1);
                        plane.data[base + y * stride] = P::from_i32(v.max(0).min(max));
                    }
                }
            } else {
                for x in 0..n {
                    let pos = (x as i32 + 1) * angle;
                    let idx = pos >> 5;
                    let fact = pos & 31;
                    let b = (off as i32 + idx + 1) as usize;
                    if fact != 0 {
                        for y in 0..n {
                            plane.data[base + y * stride + x] = P::from_i32(
                                ((32 - fact) * refm[b + y] + fact * refm[b + y + 1] + 16) >> 5,
                            );
                        }
                    } else {
                        for y in 0..n {
                            plane.data[base + y * stride + x] = P::from_i32(refm[b + y]);
                        }
                    }
                }
                if mode == 10 && is_luma && n < 32 {
                    for x in 0..n {
                        let v = left(r, n, 0) + ((top(r, n, x) - corner) >> 1);
                        plane.data[base + x] = P::from_i32(v.max(0).min(max));
                    }
                }
            }
        }
    }
}
