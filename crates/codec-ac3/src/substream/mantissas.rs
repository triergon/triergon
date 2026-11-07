//! Mantissa unpacking (7.3) including the E-AC-3 adaptive hybrid transform (Annex E
//! 3.4).

use super::{CPL, E, LFE, NoiseMode, Substream};
use crate::noise::Rng;
use crate::quant::{EXP_SCALE, GroupTables};
use crate::spec_tables::{
    VQ_HEBAP1, VQ_HEBAP2, VQ_HEBAP3, VQ_HEBAP4, VQ_HEBAP5, VQ_HEBAP6, VQ_HEBAP7,
};
use crate::tables::{
    BAP_BITS, GAQ_A1, GAQ_A2, GAQ_A4, GAQ_B2_NEG, GAQ_B2_POS, GAQ_B4_NEG, GAQ_B4_POS, HEBAP_BITS,
    frac16,
};
use triergon_bitstream::BitReader;

/// Per-block mantissa group state (7.3.5): groups are shared across exponent sets.
struct Groups {
    g1: [i32; 3],
    n1: usize,
    g2: [i32; 3],
    n2: usize,
    g4: [i32; 2],
    n4: usize,
}

impl Groups {
    fn new() -> Self {
        Groups {
            g1: [0; 3],
            n1: 0,
            g2: [0; 3],
            n2: 0,
            g4: [0; 2],
            n4: 0,
        }
    }
}

impl Substream {
    pub(super) fn unpack_mantissas(
        &mut self,
        br: &mut BitReader<'_>,
        blk: usize,
        rng: &mut Rng,
        noise: NoiseMode,
        groups: &GroupTables,
    ) -> Result<(), E> {
        self.coef_clear();
        let mut g = Groups::new();
        let mut got_cpl = false;
        for ch in 0..self.nfchans {
            if self.aht[ch] {
                if !self.aht_read[ch] {
                    self.read_aht_channel(br, ch, rng, noise)?;
                    self.aht_read[ch] = true;
                }
                self.apply_aht(ch, blk);
            } else {
                let (dith, end) = (
                    self.dithflag[ch] && noise == NoiseMode::Normal,
                    self.endmant[ch],
                );
                read_mantissas(
                    br,
                    &self.bap[ch],
                    &self.exps[ch],
                    0,
                    end,
                    dith,
                    rng,
                    groups,
                    &mut g,
                    &mut self.icoef[ch],
                );
            }
            if self.cplinu && self.chincpl[ch] && !got_cpl {
                got_cpl = true;
                if self.aht[CPL] {
                    if !self.aht_read[CPL] {
                        self.read_aht_channel(br, CPL, rng, noise)?;
                        self.aht_read[CPL] = true;
                    }
                    self.apply_aht(CPL, blk);
                } else {
                    let (s, e) = (self.cplstrtmant, self.cplendmant);
                    // Compatible coupling draws once here, even if every channel
                    // suppresses the resulting noise during decoupling.
                    read_mantissas(
                        br,
                        &self.bap[CPL],
                        &self.exps[CPL],
                        s,
                        e,
                        rng.compatible(),
                        rng,
                        groups,
                        &mut g,
                        &mut self.icoef[CPL],
                    );
                }
            }
        }
        if self.lfeon {
            if self.aht[LFE] {
                if !self.aht_read[LFE] {
                    self.read_aht_channel(br, LFE, rng, noise)?;
                    self.aht_read[LFE] = true;
                }
                self.apply_aht(LFE, blk);
            } else {
                read_mantissas(
                    br,
                    &self.bap[LFE],
                    &self.exps[LFE],
                    0,
                    7,
                    false,
                    rng,
                    groups,
                    &mut g,
                    &mut self.icoef[LFE],
                );
            }
        }
        Ok(())
    }

    fn coef_clear(&mut self) {
        let n = self.nfchans;
        for c in &mut self.icoef[..n] {
            c.fill(0);
        }
        if self.cplinu {
            self.icoef[CPL].fill(0);
        }
        if self.lfeon {
            self.icoef[LFE].fill(0);
        }
    }

    fn read_aht_channel(
        &mut self,
        br: &mut BitReader<'_>,
        ch: usize,
        rng: &mut Rng,
        noise: NoiseMode,
    ) -> Result<(), E> {
        let (start, end) = (self.strtmant[ch], self.endmant[ch]);
        let gaqmod = br.read(2) as u8;
        let endbap = if gaqmod < 2 { 12 } else { 17 };
        let hebap = &self.bap[ch];
        let mut active = 0usize;
        for bin in start..end {
            let h = hebap[bin];
            if h > 7 && h < endbap {
                active += 1;
            }
        }
        let nsections = match gaqmod {
            1 | 2 => active,
            3 => active.div_ceil(3),
            _ => 0,
        };
        if gaqmod != 0 {
            self.features.gaq = true;
        }
        // Gains in active-bin order; stored as log2 gain (0, 1, 2).
        let mut gains = [0u8; 256];
        match gaqmod {
            1 | 2 => {
                let g = if gaqmod == 1 { 1 } else { 2 };
                for v in gains.iter_mut().take(nsections) {
                    *v = if br.bit() { g } else { 0 };
                }
            }
            3 => {
                for s in 0..nsections {
                    let grp = br.read(5) as usize;
                    let m = [grp / 9, (grp % 9) / 3, grp % 3];
                    for (k, &mv) in m.iter().enumerate() {
                        if 3 * s + k < 256 {
                            gains[3 * s + k] = (mv.min(2)) as u8;
                        }
                    }
                }
            }
            _ => {}
        }
        let idct = &AHT_IDCT;
        let mut active_idx = 0usize;
        for bin in start..end {
            let h = hebap[bin] as usize;
            let mut x = [0f32; 6];
            if h == 0 {
                // No bits. FFmpeg 9.0.2 fills these DCT coefficients with uniform noise in
                // [-0.5, 0.5) independent of dithflag (black-box).
                if noise == NoiseMode::Normal {
                    for xj in &mut x {
                        *xj = rng.aht_coefficient();
                    }
                }
            } else if h < 8 {
                let bits = HEBAP_BITS[h] as u32;
                let idx = br.read(bits) as usize;
                let row: &[i16; 6] = match h {
                    1 => &VQ_HEBAP1[idx],
                    2 => &VQ_HEBAP2[idx],
                    3 => &VQ_HEBAP3[idx],
                    // FFmpeg 9.0.2 compatibility (black-box observation): FFmpeg
                    // decodes hebap-4 index i with Table E4.4 row i + 1 and index 31 as zero.
                    4 if self.reference_compat => VQ_HEBAP4.get(idx + 1).unwrap_or(&[0; 6]),
                    4 => &VQ_HEBAP4[idx],
                    5 => &VQ_HEBAP5[idx],
                    6 => &VQ_HEBAP6[idx],
                    _ => &VQ_HEBAP7[idx],
                };
                for j in 0..6 {
                    x[j] = row[j] as f32 / 32768.0;
                }
            } else {
                let m = HEBAP_BITS[h] as u32;
                let gain_log = if h < endbap as usize {
                    let g = gains[active_idx];
                    active_idx += 1;
                    g
                } else {
                    0
                };
                for xj in &mut x {
                    *xj = gaq_mantissa(br, h, m, gain_log);
                }
            }
            for (blk, row) in idct.iter().enumerate() {
                let mut s = 0f32;
                for j in 0..6 {
                    s += row[j] * x[j];
                }
                self.aht_coef[ch][blk][bin] = s * 8388608.0;
            }
        }
        Ok(())
    }

    fn apply_aht(&mut self, ch: usize, blk: usize) {
        let (start, end) = (self.strtmant[ch], self.endmant[ch]);
        let src = &self.aht_coef[ch][blk];
        for bin in start..end {
            self.icoef[ch][bin] = (src[bin].round() as i32) >> self.exps[ch][bin];
        }
    }
}

static AHT_IDCT: std::sync::LazyLock<[[f32; 6]; 6]> =
    std::sync::LazyLock::new(crate::quant::aht_idct_matrix);

/// Unpack mantissas for bins start..end (7.3) as Q23 integers shifted by the exponent.
#[allow(clippy::too_many_arguments)]
#[inline]
fn read_mantissas(
    br: &mut BitReader<'_>,
    bap: &[u8; 256],
    exps: &[u8; 256],
    start: usize,
    end: usize,
    dither: bool,
    rng: &mut Rng,
    t: &GroupTables,
    g: &mut Groups,
    out: &mut [i32; 256],
) {
    for bin in start..end {
        let b = bap[bin];
        let m = match b {
            0 => {
                if dither {
                    rng.dither_q23()
                } else {
                    0
                }
            }
            1 => {
                if g.n1 == 0 {
                    g.g1 = t.g1[br.read(5) as usize];
                    g.n1 = 3;
                }
                g.n1 -= 1;
                g.g1[2 - g.n1]
            }
            2 => {
                if g.n2 == 0 {
                    g.g2 = t.g2[br.read(7) as usize];
                    g.n2 = 3;
                }
                g.n2 -= 1;
                g.g2[2 - g.n2]
            }
            3 => t.b3[br.read(3) as usize],
            4 => {
                if g.n4 == 0 {
                    g.g4 = t.g4[br.read(7) as usize];
                    g.n4 = 2;
                }
                g.n4 -= 1;
                g.g4[1 - g.n4]
            }
            5 => t.b5[br.read(4) as usize],
            _ => {
                let bits = BAP_BITS[b as usize] as u32;
                br.read_signed(bits) << (24 - bits)
            }
        };
        out[bin] = m >> exps[bin];
    }
}

/// One gain-adaptive-quantized AHT mantissa (Annex E 3.4.4.2, Tables E3.5/E3.6).
#[inline]
fn gaq_mantissa(br: &mut BitReader<'_>, hebap: usize, m: u32, gain_log: u8) -> f32 {
    let idx = hebap - 8;
    match gain_log {
        0 => {
            let x = br.read_signed(m) as f32 * EXP_SCALE[(m - 1) as usize];
            x + frac16(GAQ_A1[idx] as u16) * x
        }
        1 => {
            let sb = m - 1;
            let s = br.read_signed(sb);
            if s == -(1 << (sb - 1)) {
                let lb = m - 1;
                let x = br.read_signed(lb) as f32 * EXP_SCALE[(lb - 1) as usize];
                let b = if x >= 0.0 {
                    frac16(GAQ_B2_POS)
                } else {
                    frac16(GAQ_B2_NEG[idx])
                };
                x + frac16(GAQ_A2[idx]) * x + b
            } else {
                s as f32 * EXP_SCALE[(sb - 1) as usize] * 0.5
            }
        }
        _ => {
            let sb = m - 2;
            let s = br.read_signed(sb);
            if s == -(1 << (sb - 1)) {
                let lb = m;
                let x = br.read_signed(lb) as f32 * EXP_SCALE[(lb - 1) as usize];
                let b = if x >= 0.0 {
                    frac16(GAQ_B4_POS)
                } else {
                    frac16(GAQ_B4_NEG[idx])
                };
                x + frac16(GAQ_A4[idx]) * x + b
            } else {
                s as f32 * EXP_SCALE[(sb - 1) as usize] * 0.25
            }
        }
    }
}
