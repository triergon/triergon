//! Separable 8×8 DCT-II / DCT-III in 32-bit fixed point with the MPEG
//! normalisation F(u,v) = ¼·C(u)·C(v)·ΣΣ f(x,y)·cos((2x+1)uπ/16)·cos((2y+1)vπ/16).
//! Each 1-D pass splits into the even (4-point) and odd (4×4 rotation) halves.

/// cos(kπ/16)·2^13 / 2 (the ½ of the 1-D normalisation folded in), and the
/// DC/X4 factor ½·cos(π/4).
const C1: i32 = 4017; // 0.5·cos(1π/16)·8192
const C2: i32 = 3784; // 0.5·cos(2π/16)·8192
const C3: i32 = 3406; // 0.5·cos(3π/16)·8192
const C4: i32 = 2896; // 0.5·cos(4π/16)·8192
const C5: i32 = 2276; // 0.5·cos(5π/16)·8192
const C6: i32 = 1567; // 0.5·cos(6π/16)·8192
const C7: i32 = 799; // 0.5·cos(7π/16)·8192
const BITS: u32 = 13;
/// Extra fraction bits carried between the two passes.
const PASS: u32 = 3;

/// Integer upper bound for every coefficient produced from this residual SAD.
/// Each one-dimensional weight has magnitude at most4096. With the actual
/// row shift10 and rounding512, the sum of eight absolute row results is at
/// most4*SAD+4. The column shift16 and rounding32768 therefore give
/// floor((4096*(4*SAD+4)+32768)/65536) = ceil(SAD/4).
/// The encoder's eight-bit residual domain also keeps both passes within i32.
#[inline]
pub(super) fn magnitude_bound(sum_abs: u32) -> u32 {
    sum_abs.div_ceil(4)
}

#[inline(always)]
fn fdct_1d(x: [i32; 8], shift: u32) -> [i32; 8] {
    let round = 1 << (shift - 1);
    let (s07, d07) = (x[0] + x[7], x[0] - x[7]);
    let (s16, d16) = (x[1] + x[6], x[1] - x[6]);
    let (s25, d25) = (x[2] + x[5], x[2] - x[5]);
    let (s34, d34) = (x[3] + x[4], x[3] - x[4]);
    let (t0, t3) = (s07 + s34, s07 - s34);
    let (t1, t2) = (s16 + s25, s16 - s25);
    [
        ((t0 + t1) * C4 + round) >> shift,
        (d07 * C1 + d16 * C3 + d25 * C5 + d34 * C7 + round) >> shift,
        (t3 * C2 + t2 * C6 + round) >> shift,
        (d07 * C3 - d16 * C7 - d25 * C1 - d34 * C5 + round) >> shift,
        ((t0 - t1) * C4 + round) >> shift,
        (d07 * C5 - d16 * C1 + d25 * C7 + d34 * C3 + round) >> shift,
        (t3 * C6 - t2 * C2 + round) >> shift,
        (d07 * C7 - d16 * C5 + d25 * C3 - d34 * C1 + round) >> shift,
    ]
}

#[inline(always)]
fn idct_1d(x: [i32; 8], shift: u32) -> [i32; 8] {
    let round = 1 << (shift - 1);
    let p = (x[0] + x[4]) * C4;
    let q = (x[0] - x[4]) * C4;
    let r = x[2] * C2 + x[6] * C6;
    let s = x[2] * C6 - x[6] * C2;
    let (e0, e3, e1, e2) = (p + r, p - r, q + s, q - s);
    let o0 = x[1] * C1 + x[3] * C3 + x[5] * C5 + x[7] * C7;
    let o1 = x[1] * C3 - x[3] * C7 - x[5] * C1 - x[7] * C5;
    let o2 = x[1] * C5 - x[3] * C1 + x[5] * C7 + x[7] * C3;
    let o3 = x[1] * C7 - x[3] * C5 + x[5] * C3 - x[7] * C1;
    [
        (e0 + o0 + round) >> shift,
        (e1 + o1 + round) >> shift,
        (e2 + o2 + round) >> shift,
        (e3 + o3 + round) >> shift,
        (e3 - o3 + round) >> shift,
        (e2 - o2 + round) >> shift,
        (e1 - o1 + round) >> shift,
        (e0 - o0 + round) >> shift,
    ]
}

/// Forward DCT of an 8×8 block of samples or residuals (raster order).
pub(crate) fn fdct(block: &mut [i32; 64]) {
    for row in block.chunks_exact_mut(8) {
        let out = fdct_1d(row.try_into().unwrap(), BITS - PASS);
        row.copy_from_slice(&out);
    }
    columns(block, BITS + PASS, fdct_1d);
}

/// Inverse DCT of dequantised coefficients (raster order), producing samples
/// or residuals before clamping.
pub(crate) fn idct(block: &mut [i32; 64]) {
    for row in block.chunks_exact_mut(8) {
        if row[1..].iter().all(|&v| v == 0) {
            let dc = (row[0] * C4 + (1 << (BITS - PASS - 1))) >> (BITS - PASS);
            row.fill(dc);
            continue;
        }
        let out = idct_1d(row.try_into().unwrap(), BITS - PASS);
        row.copy_from_slice(&out);
    }
    columns(block, BITS + PASS, idct_1d);
}

/// Apply a 1-D transform down every column. The loop body touches the same
/// lane of eight rows, so the compiler runs all columns in vector lanes.
#[inline(always)]
fn columns(block: &mut [i32; 64], shift: u32, f: impl Fn([i32; 8], u32) -> [i32; 8]) {
    let mut out = [0i32; 64];
    for c in 0..8 {
        let col = [
            block[c],
            block[8 + c],
            block[16 + c],
            block[24 + c],
            block[32 + c],
            block[40 + c],
            block[48 + c],
            block[56 + c],
        ];
        let t = f(col, shift);
        for r in 0..8 {
            out[r * 8 + c] = t[r];
        }
    }
    *block = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(u: usize, x: usize) -> f64 {
        ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos()
    }

    fn c(u: usize) -> f64 {
        if u == 0 {
            std::f64::consts::FRAC_1_SQRT_2
        } else {
            1.0
        }
    }

    fn reference_fdct(f: &[i32; 64]) -> [f64; 64] {
        std::array::from_fn(|i| {
            let (v, u) = (i / 8, i % 8);
            let mut s = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    s += f[y * 8 + x] as f64 * cos(u, x) * cos(v, y);
                }
            }
            0.25 * c(u) * c(v) * s
        })
    }

    fn reference_idct(f: &[i32; 64]) -> [f64; 64] {
        std::array::from_fn(|i| {
            let (y, x) = (i / 8, i % 8);
            let mut s = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    s += c(u) * c(v) * f[v * 8 + u] as f64 * cos(u, x) * cos(v, y);
                }
            }
            0.25 * s
        })
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, lo: i32, hi: i32) -> i32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + ((self.0 >> 33) % (hi - lo + 1) as u64) as i32
        }
    }

    #[test]
    fn forward_matches_reference_within_one() {
        let mut rng = Lcg(1);
        for _ in 0..2000 {
            let input: [i32; 64] = std::array::from_fn(|_| rng.next(-255, 255));
            let wanted = reference_fdct(&input);
            let mut got = input;
            fdct(&mut got);
            for i in 0..64 {
                assert!(
                    (got[i] as f64 - wanted[i]).abs() <= 1.0,
                    "{i}: {} vs {}",
                    got[i],
                    wanted[i]
                );
            }
        }
    }

    /// IEEE 1180-style accuracy: random coefficients from bounded pixel blocks;
    /// peak error <= 1, mean square error per position well under 0.06.
    #[test]
    fn inverse_meets_ieee1180_style_bounds() {
        let mut rng = Lcg(7);
        let mut sq = [0f64; 64];
        let n = 10000;
        for _ in 0..n {
            let pixels: [i32; 64] = std::array::from_fn(|_| rng.next(-256, 255));
            let forward = reference_fdct(&pixels);
            let coeffs: [i32; 64] =
                std::array::from_fn(|i| (forward[i].round() as i32).clamp(-2048, 2047));
            let wanted = reference_idct(&coeffs);
            let mut got = coeffs;
            idct(&mut got);
            for i in 0..64 {
                let w = wanted[i].round().clamp(-256.0, 255.0);
                let e = got[i].clamp(-256, 255) as f64 - w;
                assert!(e.abs() <= 1.0);
                sq[i] += e * e;
            }
        }
        for s in sq {
            assert!(s / n as f64 <= 0.06);
        }
    }

    #[test]
    fn constant_block_dc() {
        let mut b = [128; 64];
        fdct(&mut b);
        assert_eq!(b[0], 1024);
        assert!(b[1..].iter().all(|&v| v == 0));
        let mut r = [0; 64];
        r[0] = 1024;
        idct(&mut r);
        assert!(r.iter().all(|&v| v == 128));
    }
}
