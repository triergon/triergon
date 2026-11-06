//! Coefficient reconstruction (decoupling, rematrixing, spectral extension, dynamic
//! range) and the inverse transform with overlap-add (7.4-7.9).

use super::{BlockSink, CPL, LFE, NoiseMode, Substream};
use crate::imdct::Imdct;
use crate::noise::Rng;
use crate::tables::{REMAT_BANDS, spx_atten, spx_band_start};

impl Substream {
    pub(super) fn reconstruct(&mut self, rng: &mut Rng, noise: NoiseMode) {
        let nfch = self.nfchans;
        // Decoupling (7.4.3).
        if self.cplinu {
            let begf = self.cplbegf as usize;
            let shared_noise = rng.compatible();
            for ch in 0..nfch {
                if !self.chincpl[ch] {
                    continue;
                }
                let dith = self.dithflag[ch] && noise == NoiseMode::Normal && !self.aht[CPL];
                for sb in 0..self.ncplsubnd {
                    let bnd = self.cpl_sb_band[sb] as usize;
                    let co = self.cplco[ch][bnd] as f64 * 8.0;
                    // Phase flags negate after the coordinate product (matters for the
                    // rounding of the Q23 result; matches FFmpeg 9.0.2).
                    let flip = ch == 1 && self.acmod == 2 && self.phsflginu && self.phsflg[bnd];
                    let lo = (begf + sb) * 12 + 37;
                    for bin in lo..lo + 12 {
                        let v = if self.bap[CPL][bin] != 0 {
                            self.icoef[CPL][bin]
                        } else if shared_noise {
                            if self.dithflag[ch] {
                                self.icoef[CPL][bin]
                            } else {
                                0
                            }
                        } else if self.aht[CPL] {
                            self.icoef[CPL][bin]
                        } else if dith {
                            rng.dither_q23() >> self.exps[CPL][bin]
                        } else {
                            0
                        };
                        let p = (v as f64 * co).floor() as i32;
                        self.icoef[ch][bin] = if flip { -p } else { p };
                    }
                }
            }
        }
        // Rematrixing (7.5.4), limited to the lower bandwidth of the two channels.
        if self.acmod == 2 && self.nrematbd > 0 {
            let upper = self.endmant[0].min(self.endmant[1]);
            let (l, r) = self.icoef.split_at_mut(1);
            let (l, r) = (&mut l[0], &mut r[0]);
            for b in 0..self.nrematbd {
                if !self.rematflg[b] {
                    continue;
                }
                let lo = REMAT_BANDS[b];
                let hi = REMAT_BANDS[b + 1].min(upper);
                for bin in lo..hi {
                    let (a, c) = (l[bin], r[bin]);
                    l[bin] = a + c;
                    r[bin] = a - c;
                }
            }
        }
        // Conversion to float with the dynamic range gain (7.7.1) of each channel.
        let g0 = self.dynrng[0] * Q23_SCALE;
        let g1 = if self.acmod == 0 {
            self.dynrng[1] * Q23_SCALE
        } else {
            g0
        };
        for ch in 0..nfch {
            let g = if ch == 1 { g1 } else { g0 };
            for (d, &s) in self.coef[ch].iter_mut().zip(self.icoef[ch].iter()) {
                *d = s as f32 * g;
            }
        }
        if self.lfeon {
            for (d, &s) in self.coef[LFE].iter_mut().zip(self.icoef[LFE].iter()) {
                *d = s as f32 * g0;
            }
        }
        // Spectral extension (Annex E 3.6).
        if self.eac3 && self.spxinu {
            for ch in 0..nfch {
                if self.chinspx[ch] {
                    self.apply_spx(ch, rng, noise);
                }
            }
        }
    }

    fn apply_spx(&mut self, ch: usize, rng: &mut Rng, noise: NoiseMode) {
        let tc = &mut self.coef[ch];
        let copystart = spx_band_start(self.spxstrtf);
        let copyend = spx_band_start(self.spx_begin);
        let mut copy = copystart;
        let mut insert = copyend;
        let mut wrap = [false; 18];
        for bnd in 0..self.nspxbnds {
            let bandsize = self.spxbndsz[bnd];
            if copy + bandsize > copyend {
                copy = copystart;
                wrap[bnd] = true;
            }
            for _ in 0..bandsize {
                if copy == copyend {
                    copy = copystart;
                }
                tc[insert] = tc[copy];
                insert += 1;
                copy += 1;
            }
        }
        // Banded RMS energy of the translated coefficients.
        let mut rms = [0f32; 18];
        let mut m = copyend;
        for bnd in 0..self.nspxbnds {
            let bs = self.spxbndsz[bnd];
            let acc: f32 = tc[m..m + bs].iter().map(|v| v * v).sum();
            rms[bnd] = (acc / bs as f32).sqrt();
            m += bs;
        }
        // Band border notch filter.
        if self.chinspxatten[ch] {
            let code = self.spxattencod[ch] as usize;
            let att = [spx_atten(code, 0), spx_atten(code, 1), spx_atten(code, 2)];
            let notch = |tc: &mut [f32; 256], border: usize| {
                tc[border - 2] *= att[0];
                tc[border - 1] *= att[1];
                tc[border] *= att[2];
                tc[border + 1] *= att[1];
                tc[border + 2] *= att[0];
            };
            notch(tc, copyend);
            let mut border = copyend + self.spxbndsz[0];
            for bnd in 1..self.nspxbnds {
                if wrap[bnd] {
                    notch(tc, border);
                }
                border += self.spxbndsz[bnd];
            }
            self.features.spx_atten = true;
        }
        // Noise blending and coordinate scaling.
        let mut m = copyend;
        for bnd in 0..self.nspxbnds {
            let bs = self.spxbndsz[bnd];
            let co = self.spxco[ch][bnd] * 32.0;
            let band = &mut tc[m..m + bs];
            if rng.compatible() {
                let signal = self.sblend[ch][bnd] * co;
                let amplitude = (self.nblend[ch][bnd] * co) * rms[bnd];
                for v in band {
                    *v = *v * signal + rng.compatible_spx_noise(amplitude);
                }
            } else {
                let nscale = rms[bnd] * self.nblend[ch][bnd];
                let sscale = self.sblend[ch][bnd];
                for v in band {
                    let n = if noise == NoiseMode::Normal {
                        rng.uniform() * SQRT3
                    } else {
                        0.0
                    };
                    *v = (*v * sscale + n * nscale) * co;
                }
            }
            m += bs;
        }
    }

    /// FFmpeg 9.0.2 compatibility (observed black-box): the first audio
    /// block in which the full-bandwidth channels use different transform lengths
    /// rewrites the overlap delay lines of the coded channels before the inverse
    /// transform as if a previously down-mixed delay had to be up-mixed. A/52 does not
    /// do this; it is applied only when `DecoderOptions::compat` is `Compat::Reference`.
    pub(super) fn ffmpeg_first_mixed_transform_quirk(&mut self) {
        let d = &mut self.delay;
        match self.acmod {
            0 | 2 => d[1] = d[0],
            4 => d[2].fill(0.0),
            6 => {
                d[3].fill(0.0);
                d[2].fill(0.0);
            }
            3 | 5 | 7 => {
                if self.acmod == 7 {
                    d[4].fill(0.0);
                }
                if self.acmod >= 5 {
                    d[3].fill(0.0);
                }
                d[2] = d[1];
                d[1].fill(0.0);
            }
            _ => {}
        }
    }

    pub(super) fn synthesize(&mut self, blk: usize, imdct: &mut Imdct, sink: &mut BlockSink<'_>) {
        let chans: [usize; 6] = [0, 1, 2, 3, 4, LFE];
        for &ch in chans.iter() {
            if (ch == LFE && !self.lfeon) || (ch != LFE && ch >= self.nfchans) {
                continue;
            }
            let short = ch != LFE && self.blksw[ch];
            let out: &mut [f32] = match sink.map[ch] {
                Some(p) => {
                    let o = sink.base + blk * 256;
                    &mut sink.planes[p][o..o + 256]
                }
                None => &mut sink.scratch[..],
            };
            if short {
                imdct.short(&self.coef[ch], &mut self.delay[ch], out);
            } else {
                imdct.long(&self.coef[ch], &mut self.delay[ch], out);
            }
        }
    }
}

/// 2^-23: scale of the Q23 coefficient integers.
const Q23_SCALE: f32 = 1.0 / 8388608.0;
const SQRT3: f32 = 1.732_050_8;
