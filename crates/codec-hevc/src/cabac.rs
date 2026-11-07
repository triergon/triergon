//! CABAC context variable layout and initialization (clause 9.3.2.2, Tables 9-5 .. 9-31)
//! and the level binarization, over the shared arithmetic engine.

use triergon_bitstream::cabac::{Engine, init_state};

/// The arithmetic decoder of a slice.
pub type Cabac<'a> = Engine<'a, true>;

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
    for (s, &init) in states.iter_mut().zip(iv.iter()) {
        let slope = (init >> 4) as i32;
        let offset = (init & 15) as i32;
        *s = init_state(slope * 5 - 45, (offset << 3) - 16, slice_qp);
    }
}

/// Syntax on top of the shared engine.
pub trait Bins {
    /// True if decoding has read past the end of the data (corrupt stream).
    fn overrun(&self) -> bool;
    /// coeff_abs_level_remaining (9.3.3.11) with cRiceParam `rice`.
    fn level_remaining(&mut self, rice: u32) -> u32;
}

impl Bins for Cabac<'_> {
    #[inline]
    fn overrun(&self) -> bool {
        self.bits_read() as isize > (self.data().len() * 8) as isize + 64
    }

    /// The common case (prefix <= 3, i.e. a TR prefix plus a rice-bit suffix within the next
    /// 16 bins) is resolved from one 16-bin quotient: the quotient of the first n bins is
    /// its top n bits.
    #[inline(always)]
    fn level_remaining(&mut self, rice: u32) -> u32 {
        let q = self.peek_bypass(16);
        let prefix = (q << 16).leading_ones();
        if prefix <= 3 {
            let n = prefix + 1 + rice;
            let qn = q >> (16 - n);
            self.consume_bypass(n, qn);
            self.refill();
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
    #[ignore = "timing probe; run on demand with --release"]
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
                acc = acc.wrapping_add(c.decision(&mut st[ctx::SIG_COEFF + (i & 7)]));
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
                    acc = acc.wrapping_add(c.decision(s));
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
