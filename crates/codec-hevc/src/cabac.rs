//! CABAC arithmetic decoding engine (clause 9.3.4.3) and context variable layout and
//! initialization (clause 9.3.2.2, Tables 9-5 .. 9-31).
//!
//! The engine keeps ivlOffset scaled by 2^bits together with `bits` look-ahead bits in a
//! 64-bit window so that renormalization is a shift and refills happen once per several bytes.

use crate::tables::{RANGE_TAB_LPS, TRANS_IDX_LPS, TRANS_IDX_MPS};

/// Context offsets (one context variable per entry).
pub mod ctx {
    pub const SAO_MERGE: usize = 0;
    pub const SAO_TYPE: usize = 1;
    pub const SPLIT_CU: usize = 2; // 3
    pub const CU_TRANSQUANT_BYPASS: usize = 5;
    pub const CU_SKIP: usize = 6; // 3
    pub const PRED_MODE: usize = 9;
    pub const PART_MODE: usize = 10; // 4
    pub const PREV_INTRA_LUMA_PRED: usize = 14;
    pub const INTRA_CHROMA_PRED_MODE: usize = 15;
    pub const RQT_ROOT_CBF: usize = 16;
    pub const MERGE_FLAG: usize = 17;
    pub const MERGE_IDX: usize = 18;
    pub const INTER_PRED_IDC: usize = 19; // 5
    pub const REF_IDX: usize = 24; // 2
    pub const MVP_FLAG: usize = 26;
    pub const SPLIT_TRANSFORM: usize = 27; // 3
    pub const CBF_LUMA: usize = 30; // 2
    pub const CBF_CHROMA: usize = 32; // 5
    pub const ABS_MVD_GT0: usize = 37;
    pub const ABS_MVD_GT1: usize = 38;
    pub const CU_QP_DELTA_ABS: usize = 39; // 2
    pub const TRANSFORM_SKIP: usize = 41; // 2 (luma, chroma)
    pub const LAST_X_PREFIX: usize = 43; // 18
    pub const LAST_Y_PREFIX: usize = 61; // 18
    pub const CODED_SUB_BLOCK: usize = 79; // 4
    pub const SIG_COEFF: usize = 83; // 42
    pub const GT1: usize = 125; // 24
    pub const GT2: usize = 149; // 6
    pub const NUM: usize = 155;
}

/// initValue tables per initType (0, 1, 2), laid out to match `ctx`.
fn init_values(init_type: usize) -> [u8; ctx::NUM] {
    let mut v = [154u8; ctx::NUM];
    let t = init_type;
    let mut put = |off: usize, vals: &[u8]| {
        v[off..off + vals.len()].copy_from_slice(vals);
    };
    // Table 9-5, 9-6
    put(ctx::SAO_MERGE, &[[153], [153], [153]][t]);
    put(ctx::SAO_TYPE, &[[200], [185], [160]][t]);
    // Table 9-7
    put(
        ctx::SPLIT_CU,
        &[[139, 141, 157], [107, 139, 126], [107, 139, 126]][t],
    );
    // Table 9-8
    put(ctx::CU_TRANSQUANT_BYPASS, &[154]);
    // Table 9-9 (no initType 0)
    put(
        ctx::CU_SKIP,
        &[[154, 154, 154], [197, 185, 201], [197, 185, 201]][t],
    );
    // Table 9-10
    put(ctx::PRED_MODE, &[[154], [149], [134]][t]);
    // Table 9-11 (initType 0 has only ctxIdx 0)
    put(
        ctx::PART_MODE,
        &[
            [184, 154, 154, 154],
            [154, 139, 154, 154],
            [154, 139, 154, 154],
        ][t],
    );
    // Table 9-12
    put(ctx::PREV_INTRA_LUMA_PRED, &[[184], [154], [183]][t]);
    // Table 9-13
    put(ctx::INTRA_CHROMA_PRED_MODE, &[[63], [152], [152]][t]);
    // Table 9-14
    put(ctx::RQT_ROOT_CBF, &[[154], [79], [79]][t]);
    // Table 9-15
    put(ctx::MERGE_FLAG, &[[154], [110], [154]][t]);
    // Table 9-16
    put(ctx::MERGE_IDX, &[[154], [122], [137]][t]);
    // Table 9-17
    put(
        ctx::INTER_PRED_IDC,
        &[[154; 5], [95, 79, 63, 31, 31], [95, 79, 63, 31, 31]][t],
    );
    // Table 9-18
    put(ctx::REF_IDX, &[[154, 154], [153, 153], [153, 153]][t]);
    // Table 9-19
    put(ctx::MVP_FLAG, &[[154], [168], [168]][t]);
    // Table 9-20
    put(
        ctx::SPLIT_TRANSFORM,
        &[[153, 138, 138], [124, 138, 94], [224, 167, 122]][t],
    );
    // Table 9-21
    put(ctx::CBF_LUMA, &[[111, 141], [153, 111], [153, 111]][t]);
    // Table 9-22 (ctxIdx 0..3 + 12, 4..7 + 13, 8..11 + 14)
    put(
        ctx::CBF_CHROMA,
        &[
            [94, 138, 182, 154, 154],
            [149, 107, 167, 154, 154],
            [149, 92, 167, 154, 154],
        ][t],
    );
    // Table 9-23
    put(ctx::ABS_MVD_GT0, &[[154], [140], [169]][t]);
    put(ctx::ABS_MVD_GT1, &[[154], [198], [198]][t]);
    // Table 9-24
    put(ctx::CU_QP_DELTA_ABS, &[154, 154]);
    // Table 9-25
    put(ctx::TRANSFORM_SKIP, &[139, 139]);
    // Tables 9-26 / 9-27
    const LAST: [[u8; 18]; 3] = [
        [
            110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
        ],
        [
            125, 110, 94, 110, 95, 79, 125, 111, 110, 78, 110, 111, 111, 95, 94, 108, 123, 108,
        ],
        [
            125, 110, 124, 110, 95, 94, 125, 111, 111, 79, 125, 126, 111, 111, 79, 108, 123, 93,
        ],
    ];
    put(ctx::LAST_X_PREFIX, &LAST[t]);
    put(ctx::LAST_Y_PREFIX, &LAST[t]);
    // Table 9-28
    put(
        ctx::CODED_SUB_BLOCK,
        &[
            [91, 171, 134, 141],
            [121, 140, 61, 154],
            [121, 140, 61, 154],
        ][t],
    );
    // Table 9-29 (ctxIdx 0..41, 42..83, 84..125)
    const SIG: [[u8; 42]; 3] = [
        [
            111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125,
            141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152,
            136, 153, 136, 139, 111, 136, 139, 111,
        ],
        [
            155, 154, 139, 153, 139, 123, 123, 63, 153, 166, 183, 140, 136, 153, 154, 166, 183,
            140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 123, 123, 107, 121, 107,
            121, 167, 151, 183, 140, 151, 183, 140,
        ],
        [
            170, 154, 139, 153, 139, 123, 123, 63, 124, 166, 183, 140, 136, 153, 154, 166, 183,
            140, 136, 153, 154, 166, 183, 140, 136, 153, 154, 170, 153, 138, 138, 122, 121, 122,
            121, 167, 151, 183, 140, 151, 183, 140,
        ],
    ];
    put(ctx::SIG_COEFF, &SIG[t]);
    // Table 9-30
    const GT1: [[u8; 24]; 3] = [
        [
            140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179,
            166, 182, 140, 227, 122, 197,
        ],
        [
            154, 196, 196, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121, 136, 137, 169,
            194, 166, 167, 154, 167, 137, 182,
        ],
        [
            154, 196, 167, 167, 154, 152, 167, 182, 182, 134, 149, 136, 153, 121, 136, 122, 169,
            208, 166, 167, 154, 152, 167, 182,
        ],
    ];
    put(ctx::GT1, &GT1[t]);
    // Table 9-31
    put(
        ctx::GT2,
        &[
            [138, 153, 136, 167, 152, 152],
            [107, 167, 91, 122, 107, 167],
            [107, 167, 91, 107, 107, 167],
        ][t],
    );
    v
}

/// Initialize all context variables for SliceQpY and initType (9-4 .. 9-6). State byte is
/// (pStateIdx << 1) | valMps.
pub fn init_contexts(states: &mut [u8; ctx::NUM], slice_qp: i32, init_type: usize) {
    let iv = init_values(init_type);
    let qp = slice_qp.clamp(0, 51);
    for (s, &init) in states.iter_mut().zip(iv.iter()) {
        let slope = (init >> 4) as i32;
        let offset = (init & 15) as i32;
        let m = slope * 5 - 45;
        let n = (offset << 3) - 16;
        let pre = (((m * qp) >> 4) + n).clamp(1, 126);
        let mps = if pre <= 63 { 0 } else { 1 };
        let p = if mps == 1 { pre - 64 } else { 63 - pre };
        *s = ((p as u8) << 1) | mps as u8;
    }
}

const fn build_next(mps_path: bool) -> [u8; 128] {
    let mut t = [0u8; 128];
    let mut s = 0;
    while s < 128 {
        let p = s >> 1;
        let mps = (s & 1) as u8;
        if mps_path {
            t[s] = (TRANS_IDX_MPS[p] << 1) | mps;
        } else {
            let nm = if p == 0 { 1 - mps } else { mps };
            t[s] = (TRANS_IDX_LPS[p] << 1) | nm;
        }
        s += 1;
    }
    t
}
const NEXT_MPS: [u8; 128] = build_next(true);
const NEXT_LPS: [u8; 128] = build_next(false);
/// NEXT_STATE[(state << 1) | is_lps]
const NEXT_STATE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 128 {
        t[i * 2] = NEXT_MPS[i];
        t[i * 2 + 1] = NEXT_LPS[i];
        i += 1;
    }
    t
};

/// Flattened rangeTabLps indexed by (state >> 1) * 4 + qRangeIdx.
const LPS_FLAT: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 64 {
        let mut q = 0;
        while q < 4 {
            t[i * 4 + q] = RANGE_TAB_LPS[i][q];
            q += 1;
        }
        i += 1;
    }
    t
};

/// ivlOffset is kept at a fixed bit position: value = ivlOffset << OFF | look-ahead bits,
/// with one guard bit above for the bypass doubling. Comparisons use `range << OFF`.
const OFF: u32 = 54;

#[derive(Clone)]
pub struct Cabac<'a> {
    data: &'a [u8],
    /// Next byte to load.
    pos: usize,
    value: u64,
    /// Number of valid look-ahead bits directly below the offset.
    left: i32,
    range: u32,
}

impl<'a> Cabac<'a> {
    /// Initialize the arithmetic decoder (9.3.2.6) at byte `start` of `data`.
    pub fn new(data: &'a [u8], start: usize) -> Self {
        let mut c = Cabac {
            data,
            pos: start,
            value: 0,
            left: 0,
            range: 510,
        };
        c.reinit(start);
        c
    }

    /// Re-initialize at byte position `start` (after PCM samples or at a substream start):
    /// ivlCurrRange = 510, ivlOffset = read_bits(9).
    pub fn reinit(&mut self, start: usize) {
        self.pos = start;
        let mut v = 0u64;
        for i in 0..7 {
            v = (v << 8) | self.data.get(start + i).copied().unwrap_or(0) as u64;
        }
        self.value = v << 7;
        self.pos = start + 7;
        self.left = 56 - 9;
        self.range = 510;
    }

    #[inline(always)]
    fn refill(&mut self) {
        // Called with left < 16: append 32 bits below the current look-ahead.
        let shift = OFF as i32 - self.left - 32;
        let w = if self.pos + 4 <= self.data.len() {
            let mut b = [0u8; 4];
            b.copy_from_slice(&self.data[self.pos..self.pos + 4]);
            u32::from_be_bytes(b)
        } else {
            let mut w = 0u32;
            for i in 0..4 {
                w = (w << 8) | self.data.get(self.pos + i).copied().unwrap_or(0) as u32;
            }
            w
        };
        self.value |= (w as u64) << shift;
        self.pos += 4;
        self.left += 32;
    }

    #[inline(always)]
    fn check(&mut self) {
        if self.left < 16 {
            self.refill();
        }
    }

    /// True if decoding has read past the end of the data (corrupt stream).
    #[inline]
    pub fn overrun(&self) -> bool {
        (self.pos * 8) as isize - self.left as isize > (self.data.len() * 8) as isize + 64
    }

    /// Byte position just after the last bit read by the arithmetic decoder, rounded up to
    /// a byte boundary (used after terminate bins for PCM and substream ends).
    #[inline]
    pub fn aligned_byte_pos(&self) -> usize {
        let bitpos = self.pos * 8 - self.left as usize;
        bitpos.div_ceil(8)
    }

    /// DecodeDecision (9.3.4.3.2) with renormalization (9.3.4.3.3).
    #[inline(always)]
    pub fn decode(&mut self, state: &mut u8) -> u32 {
        let s = *state as usize;
        let lps = LPS_FLAT[((s >> 1) << 2) | ((self.range >> 6) & 3) as usize] as u32;
        let rmps = self.range - lps;
        let scaled = (rmps as u64) << OFF;
        let bin;
        if self.value < scaled {
            bin = (s & 1) as u32;
            *state = NEXT_STATE[s << 1];
            if rmps < 256 {
                self.range = rmps << 1;
                self.value <<= 1;
                self.left -= 1;
            } else {
                self.range = rmps;
            }
        } else {
            self.value -= scaled;
            bin = ((s & 1) ^ 1) as u32;
            *state = NEXT_STATE[(s << 1) | 1];
            let n = lps.leading_zeros() - 23;
            self.range = lps << n;
            self.value <<= n;
            self.left -= n as i32;
        }
        self.check();
        bin
    }

    /// DecodeBypass (9.3.4.3.4).
    #[inline(always)]
    pub fn bypass(&mut self) -> u32 {
        self.value <<= 1;
        self.left -= 1;
        let scaled = (self.range as u64) << OFF;
        let bin = if self.value >= scaled {
            self.value -= scaled;
            1
        } else {
            0
        };
        self.check();
        bin
    }

    /// Consume `n` bypass bins whose bin string is the n-bit quotient of
    /// (ivlOffset << n | next n bits) / ivlCurrRange (9.3.4.3.4 applied n times is a long
    /// division; the remainder is the new ivlOffset).
    #[inline(always)]
    fn bypass_n(&mut self, n: u32) -> u32 {
        let top = self.value >> (OFF - n);
        let r = self.range as u64;
        let q = top / r;
        let rem = top - q * r;
        let rest = self.value & ((1u64 << (OFF - n)) - 1);
        self.value = (rem << OFF) | (rest << n);
        self.left -= n as i32;
        q as u32
    }

    /// Decode `n` (<= 16) bypass bins, MSB first.
    #[inline(always)]
    pub fn bypass_bits(&mut self, n: u32) -> u32 {
        if n <= 1 {
            return if n == 1 { self.bypass() } else { 0 };
        }
        if self.left < n as i32 {
            self.refill();
        }
        let q = self.bypass_n(n);
        self.check();
        q
    }

    /// Count a unary prefix of bypass 1-bins terminated by a 0-bin (the 0 is consumed),
    /// at most `max` ones. Peeks 8 bins per division and consumes only what is used.
    #[inline(always)]
    pub fn bypass_unary(&mut self, max: u32) -> u32 {
        let mut count = 0;
        loop {
            let top = self.value >> (OFF - 8);
            let q = (top / self.range as u64) as u32;
            let ones = (q as u8).leading_ones();
            if ones < 8 {
                self.bypass_n(ones + 1);
                self.check();
                return (count + ones).min(max);
            }
            self.bypass_n(8);
            self.check();
            count += 8;
            if count >= max {
                return max;
            }
        }
    }

    /// Consume n bypass bins whose quotient `q` is already known (see `bypass_n`).
    #[inline(always)]
    fn consume_n(&mut self, n: u32, q: u32) {
        let top = self.value >> (OFF - n);
        let rem = top - q as u64 * self.range as u64;
        let rest = self.value & ((1u64 << (OFF - n)) - 1);
        self.value = (rem << OFF) | (rest << n);
        self.left -= n as i32;
    }

    /// coeff_abs_level_remaining (9.3.3.11) with cRiceParam `rice`. The common case (prefix
    /// <= 3, i.e. a TR prefix plus a rice-bit suffix within the next 16 bins) is resolved
    /// from one 16-bin quotient: the quotient of the first n bins is its top n bits.
    #[inline(always)]
    pub fn level_remaining(&mut self, rice: u32) -> u32 {
        let top = self.value >> (OFF - 16);
        let q = (top / self.range as u64) as u32;
        let prefix = (q << 16).leading_ones();
        if prefix <= 3 {
            let n = prefix + 1 + rice;
            let qn = q >> (16 - n);
            self.consume_n(n, qn);
            self.check();
            return (prefix << rice) + (qn & ((1 << rice) - 1));
        }
        let prefix = self.bypass_unary(32);
        if prefix <= 3 {
            (prefix << rice) + self.bypass_bits(rice)
        } else {
            let nb = prefix - 3 + rice;
            let suf = if nb > 16 {
                let hi = self.bypass_bits(nb - 16);
                (hi << 16) | self.bypass_bits(16)
            } else {
                self.bypass_bits(nb)
            };
            (((1u32 << (prefix - 3).min(31)) + 2) << rice).wrapping_add(suf)
        }
    }

    /// DecodeTerminate (9.3.4.3.5).
    #[inline]
    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        let scaled = (self.range as u64) << OFF;
        if self.value >= scaled {
            1
        } else {
            if self.range < 256 {
                self.range <<= 1;
                self.value <<= 1;
                self.left -= 1;
                self.check();
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_state_example() {
        let mut s = [0u8; ctx::NUM];
        init_contexts(&mut s, 26, 0);
        // initValue 154 -> m=0, n=64 -> pre=64 -> mps 1, p 0.
        assert_eq!(s[ctx::CU_TRANSQUANT_BYPASS], 1);
    }

    #[test]
    fn bypass_reads_raw_bits_when_range_max() {
        // With range 510 bypass is not exactly raw bits, but decoding must not panic and
        // terminate on zero data must return 0.
        let data = [0u8; 16];
        let mut c = Cabac::new(&data, 0);
        for _ in 0..40 {
            assert_eq!(c.bypass(), 0);
        }
        assert_eq!(c.terminate(), 0);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Development micro-benchmark.
    #[test]
    #[ignore]
    fn bench_cabac() {
        let mut seed = 99u32;
        let data: Vec<u8> = (0..1 << 20)
            .map(|_| {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let mut states = [0u8; ctx::NUM];
        init_contexts(&mut states, 30, 0);
        let n = 4_000_000usize;
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut c = Cabac::new(&data, 0);
            let mut st = states;
            let t = std::time::Instant::now();
            let mut acc = 0u32;
            for i in 0..n {
                acc = acc.wrapping_add(c.decode(&mut st[ctx::SIG_COEFF + (i & 7)]));
            }
            std::hint::black_box(acc);
            best = best.min(t.elapsed().as_secs_f64() / n as f64);
        }
        eprintln!("decode: {:.2} ns/bin", best * 1e9);
        // Skewed: arithmetic-encode-free proxy: data with long zero runs gives mostly MPS for
        // contexts adapted to 0.
        let skew: Vec<u8> = data
            .iter()
            .enumerate()
            .map(|(i, &b)| if i % 8 == 0 { b } else { 0 })
            .collect();
        for name in ["decode skew"] {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let mut c = Cabac::new(&skew, 0);
                let mut st = states;
                let t = std::time::Instant::now();
                let mut acc = 0u32;
                for i in 0..n {
                    let s = &mut st[ctx::SIG_COEFF + (i & 7)];
                    acc = acc.wrapping_add(c.decode(s));
                }
                std::hint::black_box(acc);
                best = best.min(t.elapsed().as_secs_f64() / n as f64);
            }
            eprintln!("{name}: {:.2} ns/bin", best * 1e9);
        }
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut c = Cabac::new(&data, 0);
            let t = std::time::Instant::now();
            let mut acc = 0u32;
            for _ in 0..n {
                acc = acc.wrapping_add(c.bypass());
            }
            std::hint::black_box(acc);
            best = best.min(t.elapsed().as_secs_f64() / n as f64);
        }
        eprintln!("bypass: {:.2} ns/bin", best * 1e9);
        let mut best = f64::MAX;
        for _ in 0..5 {
            let mut c = Cabac::new(&data, 0);
            let t = std::time::Instant::now();
            let mut acc = 0u32;
            for _ in 0..n / 8 {
                acc = acc.wrapping_add(c.bypass_bits(8));
            }
            std::hint::black_box(acc);
            best = best.min(t.elapsed().as_secs_f64() / n as f64);
        }
        eprintln!("bypass_bits(8): {:.2} ns/bin", best * 1e9);
    }
}
