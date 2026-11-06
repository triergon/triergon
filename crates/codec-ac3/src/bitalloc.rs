//! Parametric bit allocation, A/52:2018 7.2 (and Annex E 3.4.3 for hebap).
//! Integer arithmetic exactly as specified.

use crate::spec_tables::{BAPTAB, BNDSZ, BNDTAB, HEBAPTAB, HTH_0, HTH_1, HTH_2, LATAB, MASKTAB};
use crate::tables::{DBPBTAB, FASTDEC, FLOORTAB, SLOWDEC, SLOWGAIN};

/// Bit-allocation parametric information (baie fields).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BaParams {
    pub sdcycod: u8,
    pub fdcycod: u8,
    pub sgaincod: u8,
    pub dbpbcod: u8,
    pub floorcod: u8,
}

/// Delta bit allocation for one channel (A/52:2018 5.4.3.47 ff., 7.2.2.6).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Dba {
    pub active: bool,
    pub nseg: usize,
    pub offst: [u8; 8],
    pub len: [u8; 8],
    pub ba: [u8; 8],
}

/// Inputs of the excitation / masking stages for one exponent set.
pub(crate) struct MaskParams<'a> {
    pub start: usize,
    pub end: usize,
    pub fscod: usize,
    /// AC-3 bsid 9/10 sample-rate shift (1 or 2; 0 otherwise): decays >> shift and
    /// hth[fscod][band >> shift].
    pub rate_shift: u32,
    pub params: BaParams,
    pub fgain: i32,
    /// Coupling channel fast/slow leak initialisation (cplfleak, cplsleak).
    pub cpl_leak: Option<(u8, u8)>,
    pub dba: &'a Dba,
}

/// Per-channel intermediate results, so that a block which only changes SNR offsets
/// re-runs only the final stage (7.2.2.7), and one that only changes parameters re-runs
/// only the masking stages (7.2.2.4-6).
#[derive(Debug, Clone)]
pub(crate) struct BaCache {
    psd: [i32; 256],
    bndpsd: [i32; 50],
    /// Masking curve after delta bit allocation, before the SNR offset.
    mask: [i32; 50],
}

impl Default for BaCache {
    fn default() -> Self {
        BaCache {
            psd: [0; 256],
            bndpsd: [0; 50],
            mask: [0; 50],
        }
    }
}

#[inline]
fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let address = ((c.abs() >> 1).min(255)) as usize;
    if c >= 0 {
        a + LATAB[address] as i32
    } else {
        b + LATAB[address] as i32
    }
}

#[inline]
fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// 7.2.2.2 / 7.2.2.3: exponent mapping into PSD and PSD integration.
pub(crate) fn stage_psd(
    exps: &[u8; 256],
    start: usize,
    end: usize,
    c: &mut BaCache,
) -> Result<(), &'static str> {
    if start >= end || end > 253 {
        return Err("bit allocation range");
    }
    for bin in start..end {
        c.psd[bin] = 3072 - ((exps[bin] as i32) << 7);
    }
    let psd = &c.psd;
    let mut j = start;
    let mut k = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[k] as usize + BNDSZ[k] as usize).min(end);
        let mut acc = psd[j];
        j += 1;
        while j < lastbin {
            acc = logadd(acc, psd[j]);
            j += 1;
        }
        c.bndpsd[k] = acc;
        k += 1;
        if end <= lastbin {
            break;
        }
    }
    Ok(())
}

/// 7.2.2.4 - 7.2.2.6: excitation function, masking curve, delta bit allocation.
pub(crate) fn stage_mask(p: &MaskParams<'_>, c: &mut BaCache) -> Result<(), &'static str> {
    let (start, end) = (p.start, p.end);
    let bndpsd = &c.bndpsd;
    // Reduced-rate AC-3 (bsid 9/10): each band spans 1/2 or 1/4 of the frequency range, so the
    // per-band decays are shifted down and hth is read at band >> shift. Not in A/52:2018; this is
    // FFmpeg 9.0.2's rule, fitted black-box bin for bin.
    let sdecay = SLOWDEC[p.params.sdcycod as usize] >> p.rate_shift;
    let fdecay = FASTDEC[p.params.fdcycod as usize] >> p.rate_shift;
    let sgain = SLOWGAIN[p.params.sgaincod as usize];
    let dbknee = DBPBTAB[p.params.dbpbcod as usize];
    let fgain = p.fgain;
    let bndstrt = MASKTAB[start] as usize;
    let bndend = MASKTAB[end - 1] as usize + 1;
    let mut excite = [0i32; 50];
    let mut fastleak;
    let mut slowleak;
    let begin;
    if bndstrt == 0 {
        let lfe_last = bndend == 7;
        let mut lowcomp = 0;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        let mut b = 7;
        fastleak = 0;
        slowleak = 0;
        for bin in 2..7 {
            if !(lfe_last && bin == 6) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - sgain;
            excite[bin] = fastleak - lowcomp;
            if !(lfe_last && bin == 6) && bndpsd[bin] <= bndpsd[bin + 1] {
                b = bin + 1;
                break;
            }
        }
        for bin in b..bndend.min(22) {
            if !(lfe_last && bin == 6) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak -= fdecay;
            fastleak = fastleak.max(bndpsd[bin] - fgain);
            slowleak -= sdecay;
            slowleak = slowleak.max(bndpsd[bin] - sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }
        begin = 22;
    } else {
        let (fl, sl) = p.cpl_leak.unwrap_or((0, 0));
        fastleak = ((fl as i32) << 8) + 768;
        slowleak = ((sl as i32) << 8) + 768;
        begin = bndstrt;
    }
    for bin in begin..bndend {
        fastleak -= fdecay;
        fastleak = fastleak.max(bndpsd[bin] - fgain);
        slowleak -= sdecay;
        slowleak = slowleak.max(bndpsd[bin] - sgain);
        excite[bin] = fastleak.max(slowleak);
    }

    let hth: &[u16; 50] = match p.fscod {
        0 => &HTH_0,
        1 => &HTH_1,
        _ => &HTH_2,
    };
    let mask = &mut c.mask;
    for bin in bndstrt..bndend {
        let mut e = excite[bin];
        if bndpsd[bin] < dbknee {
            e += (dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = e.max(hth[bin >> p.rate_shift] as i32);
    }

    if p.dba.active {
        // Segment offsets count from the first band of the exponent set: band 0 for fbw
        // channels, the coupling channel's first band for cpldeltoffst (5.4.3.51 "the number
        // of the first bit allocation band of the coupling channel"; the 7.2.2.6 pseudo
        // code starts at 0). FFmpeg 9.0.2 uses this reading too (black-box).
        let mut band = bndstrt;
        for seg in 0..=p.dba.nseg {
            band += p.dba.offst[seg] as usize;
            let ba = p.dba.ba[seg] as i32;
            let delta = if ba >= 4 {
                (ba - 3) << 7
            } else {
                (ba - 4) << 7
            };
            let len = p.dba.len[seg] as usize;
            if band + len > 50 {
                return Err("delta bit allocation band out of range");
            }
            for m in &mut mask[band..band + len] {
                *m += delta;
            }
            band += len;
        }
    }
    Ok(())
}

/// 7.2.2.7 (and Annex E 3.4.3.1): bit allocation pointers from PSD and masking curve.
pub(crate) fn stage_bap(
    c: &BaCache,
    start: usize,
    end: usize,
    snroffset: i32,
    floorcod: u8,
    hebap: bool,
    bap: &mut [u8; 256],
) {
    let floor = FLOORTAB[floorcod as usize];
    let table: &[u8; 64] = if hebap { &HEBAPTAB } else { &BAPTAB };
    let mut i = start;
    let mut j = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[j] as usize + BNDSZ[j] as usize).min(end);
        let mut m = c.mask[j] - snroffset - floor;
        if m < 0 {
            m = 0;
        }
        m &= 0x1fe0;
        m += floor;
        for (b, &p) in bap[i..lastbin].iter_mut().zip(c.psd[i..lastbin].iter()) {
            *b = table[((p - m) >> 5).clamp(0, 63) as usize];
        }
        i = lastbin;
        j += 1;
        if end <= lastbin {
            break;
        }
    }
}
