//! CABAC context layout and binarizations (clause 9.3) over the shared arithmetic engine.

use crate::tables_cabac_init::CABAC_INIT_MN;
use triergon_bitstream::cabac::init_state;

type Engine<'a> = triergon_bitstream::cabac::Engine<'a, false>;

pub(crate) const NUM_CTX: usize = 1024;

pub(crate) struct Cabac<'a> {
    e: Engine<'a>,
    /// Payload bytes of the slice, for the overrun check.
    end: usize,
    /// Context states: (pStateIdx << 1) | valMPS.
    pub ctx: [u8; NUM_CTX],
}

#[inline(always)]
fn decide(e: &mut Engine<'_>, ctx: &mut [u8; NUM_CTX], ctx_idx: usize) -> u32 {
    e.decision(&mut ctx[ctx_idx & (NUM_CTX - 1)])
}

/// `decide` without the look-ahead refill.
#[inline(always)]
fn decide_nr(e: &mut Engine<'_>, ctx: &mut [u8; NUM_CTX], ctx_idx: usize) -> u32 {
    e.decision_unfilled(&mut ctx[ctx_idx & (NUM_CTX - 1)])
}

/// Context index offsets (Table 9-34).
pub(crate) mod off {
    pub const MB_TYPE_I: usize = 3;
    pub const MB_SKIP_P: usize = 11;
    pub const MB_TYPE_P: usize = 14;
    pub const MB_TYPE_P_INTRA: usize = 17;
    pub const SUB_MB_P: usize = 21;
    pub const MB_SKIP_B: usize = 24;
    pub const MB_TYPE_B: usize = 27;
    pub const MB_TYPE_B_INTRA: usize = 32;
    pub const SUB_MB_B: usize = 36;
    pub const MVD_X: usize = 40;
    pub const MVD_Y: usize = 47;
    pub const REF_IDX: usize = 54;
    pub const QP_DELTA: usize = 60;
    pub const CHROMA_PRED: usize = 64;
    pub const PREV_INTRA: usize = 68;
    pub const REM_INTRA: usize = 69;
    pub const FIELD: usize = 70;
    pub const CBP_LUMA: usize = 73;
    pub const CBP_CHROMA: usize = 77;
    pub const CBF: usize = 85;
    pub const SIG_FRAME: usize = 105;
    pub const LAST_FRAME: usize = 166;
    pub const ABS_LEVEL: usize = 227;
    pub const SIG_FIELD: usize = 277;
    pub const LAST_FIELD: usize = 338;
    pub const T8X8: usize = 399;
    pub const SIG8_FRAME: usize = 402;
    pub const LAST8_FRAME: usize = 417;
    pub const ABS8: usize = 426;
    pub const SIG8_FIELD: usize = 436;
    pub const LAST8_FIELD: usize = 451;
}

/// ctxBlockCatOffset for coded_block_flag, significant/last and abs level (Table 9-40), cats 0..4.
pub(crate) const CBF_CAT_OFF: [usize; 5] = [0, 4, 8, 12, 16];
pub(crate) const SIG_CAT_OFF: [usize; 5] = [0, 15, 29, 44, 47];
pub(crate) const ABS_CAT_OFF: [usize; 5] = [0, 10, 20, 30, 39];

/// Table 9-43 (ctxBlockCat 5): significant_coeff_flag frame / field and last_significant_coeff_flag.
pub(crate) const SIG8_FRAME_INC: [u8; 63] = [
    0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5, 4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9, 10, 9, 8,
    7, 7, 6, 11, 12, 13, 11, 6, 7, 8, 9, 14, 10, 9, 8, 6, 11, 12, 13, 11, 6, 9, 14, 10, 9, 11, 12,
    13, 11, 14, 10, 12,
];
pub(crate) const SIG8_FIELD_INC: [u8; 63] = [
    0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 7, 7, 7, 8, 4, 5, 6, 9, 10, 10, 8, 11, 12, 11, 9, 9, 10, 10, 8,
    11, 12, 11, 9, 9, 10, 10, 8, 11, 12, 11, 9, 9, 10, 10, 8, 13, 13, 9, 9, 10, 10, 8, 13, 13, 9,
    9, 10, 10, 14, 14, 14, 14, 14,
];
pub(crate) const LAST8_INC: [u8; 63] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8,
];

impl<'a> Cabac<'a> {
    /// Initialise at byte offset `start` of `data` (clause 9.3.1.1 / 9.3.1.2). `data` must be
    /// padded with zeros past `end`.
    pub(crate) fn new(
        data: &'a [u8],
        start: usize,
        end: usize,
        slice_qp: i32,
        init_idx: usize,
    ) -> Self {
        let mut c = Cabac {
            e: Engine::new(data, start),
            end,
            ctx: [0; NUM_CTX],
        };
        c.init_contexts(slice_qp, init_idx);
        c
    }

    fn init_contexts(&mut self, slice_qp: i32, init_idx: usize) {
        for i in 0..NUM_CTX {
            let (m, n) = CABAC_INIT_MN[i][init_idx];
            self.ctx[i] = init_state(m as i32, n as i32, slice_qp);
        }
        // ctxIdx 276 is handled by decode_terminate.
    }

    /// Re-initialise the engine at a byte position (after I_PCM samples).
    pub(crate) fn reinit_at(&mut self, byte_pos: usize) {
        self.e.restart(byte_pos);
    }
    /// Byte position following the bits consumed by the engine (for I_PCM: after decoding the
    /// terminate bin, pcm_alignment_zero_bits follow up to the next byte boundary).
    pub(crate) fn pcm_byte_pos(&self) -> usize {
        self.e.aligned_byte_position()
    }
    pub(crate) fn overrun(&self) -> bool {
        self.e.next_byte() > self.end + 8
    }
    #[inline(always)]
    pub(crate) fn decision(&mut self, ctx_idx: usize) -> u32 {
        decide(&mut self.e, &mut self.ctx, ctx_idx)
    }
    #[inline(always)]
    pub(crate) fn bypass(&mut self) -> u32 {
        self.e.bypass()
    }
    #[inline(always)]
    pub(crate) fn terminate(&mut self) -> u32 {
        self.e.terminate()
    }

    // ---- binarizations ----

    /// mb_type in I slices (offset 3) or the intra suffix in P/B slices. Returns the I mb_type
    /// value (0 = I_NxN, 1..=24 = I_16x16, 25 = I_PCM).
    pub(crate) fn mb_type_i(&mut self, inc0: usize, suffix_off: Option<usize>) -> u32 {
        match suffix_off {
            None => {
                let o = off::MB_TYPE_I;
                if self.decision(o + inc0) == 0 {
                    return 0;
                }
                if self.terminate() == 1 {
                    return 25;
                }
                let cbp_l = self.decision(o + 3);
                let c0 = self.decision(o + 4);
                let cbp_c = if c0 == 0 { 0 } else { 1 + self.decision(o + 5) };
                let p0 = self.decision(o + 6);
                let p1 = self.decision(o + 7);
                1 + (p0 * 2 + p1) + 4 * cbp_c + 12 * cbp_l
            }
            Some(o) => {
                if self.decision(o) == 0 {
                    return 0;
                }
                if self.terminate() == 1 {
                    return 25;
                }
                let cbp_l = self.decision(o + 1);
                let c0 = self.decision(o + 2);
                let cbp_c = if c0 == 0 { 0 } else { 1 + self.decision(o + 2) };
                let p0 = self.decision(o + 3);
                let p1 = self.decision(o + 3);
                1 + (p0 * 2 + p1) + 4 * cbp_c + 12 * cbp_l
            }
        }
    }

    /// mb_type in P slices: returns P mb_type 0..=3, or 5 + I mb_type for intra.
    pub(crate) fn mb_type_p(&mut self) -> u32 {
        let o = off::MB_TYPE_P;
        if self.decision(o) == 0 {
            if self.decision(o + 1) == 0 {
                if self.decision(o + 2) == 0 { 0 } else { 3 }
            } else if self.decision(o + 3) == 0 {
                2
            } else {
                1
            }
        } else {
            5 + self.mb_type_i(0, Some(off::MB_TYPE_P_INTRA))
        }
    }

    /// mb_type in B slices: returns B mb_type 0..=22, or 23 + I mb_type for intra.
    pub(crate) fn mb_type_b(&mut self, inc0: usize) -> u32 {
        let o = off::MB_TYPE_B;
        if self.decision(o + inc0) == 0 {
            return 0;
        }
        if self.decision(o + 3) == 0 {
            return 1 + self.decision(o + 5);
        }
        let b2 = self.decision(o + 4);
        let b3 = self.decision(o + 5);
        let b4 = self.decision(o + 5);
        let b5 = self.decision(o + 5);
        let bits = (b2 << 3) | (b3 << 2) | (b4 << 1) | b5;
        if bits < 8 {
            return bits + 3;
        }
        match bits {
            13 => 23 + self.mb_type_i(0, Some(off::MB_TYPE_B_INTRA)),
            14 => 11,
            15 => 22,
            _ => {
                let b6 = self.decision(o + 5);
                ((bits << 1) | b6) - 4
            }
        }
    }

    pub(crate) fn sub_mb_type_p(&mut self) -> u32 {
        let o = off::SUB_MB_P;
        if self.decision(o) == 1 {
            return 0;
        }
        if self.decision(o + 1) == 0 {
            return 1;
        }
        if self.decision(o + 2) == 1 { 2 } else { 3 }
    }

    pub(crate) fn sub_mb_type_b(&mut self) -> u32 {
        let o = off::SUB_MB_B;
        if self.decision(o) == 0 {
            return 0;
        }
        if self.decision(o + 1) == 0 {
            return 1 + self.decision(o + 3);
        }
        if self.decision(o + 2) == 1 {
            if self.decision(o + 3) == 1 {
                return 11 + self.decision(o + 3);
            }
            let b4 = self.decision(o + 3);
            let b5 = self.decision(o + 3);
            return 7 + (b4 << 1) + b5;
        }
        let b3 = self.decision(o + 3);
        let b4 = self.decision(o + 3);
        3 + (b3 << 1) + b4
    }

    /// ref_idx_lX: unary with ctxIdxInc 0..3 for bin 0, 4 for bin 1, 5 after.
    pub(crate) fn ref_idx(&mut self, inc0: usize) -> u32 {
        let o = off::REF_IDX;
        if self.decision(o + inc0) == 0 {
            return 0;
        }
        if self.decision(o + 4) == 0 {
            return 1;
        }
        let mut v = 2;
        while self.decision(o + 5) == 1 {
            v += 1;
            if v > 63 {
                break;
            }
        }
        v
    }

    /// mvd_lX component (UEG3, signed, uCoff = 9).
    pub(crate) fn mvd(&mut self, o: usize, inc0: usize) -> i32 {
        if self.decision(o + inc0) == 0 {
            return 0;
        }
        let mut prefix = 1;
        let incs = [3usize, 4, 5, 6, 6, 6, 6, 6];
        while prefix < 9 && self.decision(o + incs[prefix - 1]) == 1 {
            prefix += 1;
        }
        let mut v = prefix as i32;
        if prefix >= 9 {
            // Exp-Golomb k = 3 suffix in bypass mode.
            let mut k = 3;
            while self.bypass() == 1 {
                v += 1 << k;
                k += 1;
                if k > 24 {
                    break;
                }
            }
            let mut s = 0;
            while k > 0 {
                k -= 1;
                s |= (self.bypass() as i32) << k;
            }
            v += s;
        }
        if self.bypass() == 1 { -v } else { v }
    }

    pub(crate) fn mb_qp_delta(&mut self, inc0: usize) -> i32 {
        let o = off::QP_DELTA;
        if self.decision(o + inc0) == 0 {
            return 0;
        }
        let mut k = 1;
        if self.decision(o + 2) == 1 {
            k = 2;
            while self.decision(o + 3) == 1 {
                k += 1;
                if k > 200 {
                    break;
                }
            }
        }
        if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) }
    }

    pub(crate) fn intra_chroma_pred_mode(&mut self, inc0: usize) -> u32 {
        let o = off::CHROMA_PRED;
        if self.decision(o + inc0) == 0 {
            return 0;
        }
        if self.decision(o + 3) == 0 {
            return 1;
        }
        if self.decision(o + 3) == 0 { 2 } else { 3 }
    }

    /// prev_intra_pred_mode_flag + rem_intra_pred_mode. Returns None for "use predicted".
    pub(crate) fn intra_pred_mode(&mut self) -> Option<u32> {
        if self.decision(off::PREV_INTRA) == 1 {
            return None;
        }
        let b0 = self.decision(off::REM_INTRA);
        let b1 = self.decision(off::REM_INTRA);
        let b2 = self.decision(off::REM_INTRA);
        Some(b0 | (b1 << 1) | (b2 << 2))
    }

    /// Decode the significance map and levels of one block (clause 9.3.2.7 / 9.3.3.1.3).
    /// `cat` is ctxBlockCat (0..=5), writes levels by scan position into `out[0..max]`.
    /// Returns the number of non-zero coefficients.
    #[inline(never)]
    pub(crate) fn residual(&mut self, cat: usize, field: bool, max: usize, out: &mut [i32]) -> u32 {
        let mut eng = self.e;
        let ctx = &mut self.ctx;
        let e = &mut eng;
        // Specialise the block decoder per category / scan so offsets and maxima are constants.
        let n = match (cat, field) {
            (0, false) => residual_inner(e, ctx, 0, false, 16, out),
            (1, false) => residual_inner(e, ctx, 1, false, 15, out),
            (2, false) => residual_inner(e, ctx, 2, false, 16, out),
            (3, false) => residual_inner(e, ctx, 3, false, 4, out),
            (4, false) => residual_inner(e, ctx, 4, false, 15, out),
            (5, false) => residual_inner(e, ctx, 5, false, 64, out),
            (0, true) => residual_inner(e, ctx, 0, true, 16, out),
            (1, true) => residual_inner(e, ctx, 1, true, 15, out),
            (2, true) => residual_inner(e, ctx, 2, true, 16, out),
            (3, true) => residual_inner(e, ctx, 3, true, 4, out),
            (4, true) => residual_inner(e, ctx, 4, true, 15, out),
            _ => residual_inner(e, ctx, cat, field, max, out),
        };
        self.e = eng;
        n
    }
}

#[inline(always)]
fn residual_inner(
    e: &mut Engine<'_>,
    ctx: &mut [u8; NUM_CTX],
    cat: usize,
    field: bool,
    max: usize,
    out: &mut [i32],
) -> u32 {
    {
        let mut sig_pos = [0u8; 64];
        let mut n = 0usize;
        let last = max - 1;
        if cat == 5 {
            let (so, lo) = if field {
                (off::SIG8_FIELD, off::LAST8_FIELD)
            } else {
                (off::SIG8_FRAME, off::LAST8_FRAME)
            };
            let sinc = if field {
                &SIG8_FIELD_INC
            } else {
                &SIG8_FRAME_INC
            };
            let mut i = 0;
            while i < last {
                e.refill();
                if decide_nr(e, ctx, so + sinc[i] as usize) == 1 {
                    sig_pos[n] = i as u8;
                    n += 1;
                    if decide_nr(e, ctx, lo + LAST8_INC[i] as usize) == 1 {
                        break;
                    }
                }
                i += 1;
            }
            e.refill();
            if i == last {
                sig_pos[n] = last as u8;
                n += 1;
            }
        } else {
            let (so, lo) = if field {
                (off::SIG_FIELD, off::LAST_FIELD)
            } else {
                (off::SIG_FRAME, off::LAST_FRAME)
            };
            let so = so + SIG_CAT_OFF[cat];
            let lo = lo + SIG_CAT_OFF[cat];
            let mut i = 0;
            while i < last {
                let inc = if cat == 3 { i.min(2) } else { i };
                e.refill();
                if decide_nr(e, ctx, so + inc) == 1 {
                    sig_pos[n] = i as u8;
                    n += 1;
                    if decide_nr(e, ctx, lo + inc) == 1 {
                        break;
                    }
                }
                i += 1;
            }
            e.refill();
            if i == last {
                sig_pos[n] = last as u8;
                n += 1;
            }
        }
        let ao = if cat == 5 {
            off::ABS8
        } else {
            off::ABS_LEVEL + ABS_CAT_OFF[cat]
        };
        let gt1_max = if cat == 3 { 3 } else { 4 };
        let mut eq1 = 0usize;
        let mut gt1 = 0usize;
        for k in (0..n).rev() {
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) };
            let mut abs;
            if decide(e, ctx, ao + inc0) == 0 {
                abs = 1;
            } else {
                let inc = 5 + gt1.min(gt1_max);
                let mut prefix = 1;
                while prefix < 14 && decide(e, ctx, ao + inc) == 1 {
                    prefix += 1;
                }
                abs = prefix + 1;
                if prefix >= 14 {
                    // Exp-Golomb k = 0 suffix.
                    let mut kk = 0;
                    while e.bypass() == 1 {
                        abs += 1 << kk;
                        kk += 1;
                        if kk > 24 {
                            break;
                        }
                    }
                    let mut s = 0;
                    while kk > 0 {
                        kk -= 1;
                        s |= (e.bypass() as i32) << kk;
                    }
                    abs += s;
                }
            }
            if abs == 1 {
                eq1 += 1;
            } else {
                gt1 += 1;
            }
            let v = if e.bypass() == 1 { -abs } else { abs };
            out[(sig_pos[k] & 63) as usize] = v;
        }
        n as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_states_in_range() {
        let data = [0u8; 32];
        for idx in 0..4 {
            for qp in [0, 26, 51] {
                let c = Cabac::new(&data, 0, 16, qp, idx);
                assert!(c.ctx.iter().all(|&s| (s >> 1) <= 62));
            }
        }
    }
}
