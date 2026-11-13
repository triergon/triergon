//! Parametric Stereo synthesis in the hybrid domain: the hybrid analysis of the low QMF
//! bands, the decorrelator with its transient ducking, the mixing of the mono and the
//! decorrelated signal into two channels, and the hybrid synthesis.
//!
//! Structure (H. Purnhagen, "Low complexity parametric stereo coding in MPEG-4", DAFx
//! 2004, sections 3.2, 3.3 and 4.1; 3GPP TS 26.405 V19.0.0 section 5.2.2): the lowest QMF
//! bands are split by 13-tap complex-modulated filters G(n) = g(n) exp(j 2 pi / Q (q + 1/2)
//! (n - 6)) whose delay of 6 slots is taken up by the 6 look-ahead slots SBR hands over,
//! so every band is processed at zero delay; synthesis adds the sub-bands of a QMF band.
//! The decorrelator runs a chain of three all-pass sections of 3, 4 and 5 slots with
//! fractional delays (phase rotations at each band's frequency) below QMF band 23, a delay
//! of 14 slots up to band 34 and of one slot above. Observed behaviour, given by no
//! available text and documented with its probes where it is implemented: the lead delay
//! and phase before the sections ([`Kind::AllPass`]), the taper of the coefficients over
//! the QMF bands ([`layout`]) and the transient ducking, which attenuates the decorrelated
//! signal per parameter band after power drops ([`Synth::ducking`]). The constants,
//! prototypes and band maps are recovered from the reference decoder's output
//! (`harness/codecs/aac/ps`, tables in `tables.rs`).

use super::tables as t;
use crate::error::Result;
use crate::fft::Cpx;
use std::sync::OnceLock;
use triergon_codec::{Budget, BudgetVec};

#[cfg(test)]
#[path = "synth_tests.rs"]
mod tests;

/// Slots per frame, and the look-ahead the hybrid filters read.
pub(super) const SLOTS: usize = 32;
const AHEAD: usize = 6;
/// Hybrid bands of the larger configuration (32 split + 59 QMF bands), split QMF bands.
pub(super) const MAX_HYBRID: usize = 91;
const MAX_LOW: usize = 32;
const MAX_SPLIT: usize = 5;
/// Parameter bands of the larger configuration.
pub(super) const MAX_PAR: usize = 34;

/// How one hybrid band is decorrelated.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A lead delay, then three all-pass sections.
    AllPass {
        /// Phase factor exp(-i pi `FRACT0` f) of the lead delay of `LEAD_DELAY` slots.
        /// Observed behaviour, given by no available text (Purnhagen section 4.1 has the
        /// sections only): probes d06 (a lead of 2 slots fits to -114 dB, of 1 or 3 slots to
        /// -46 dB), d08, d10 and d25 (`FRACT0`), rerun by `signal/dec/recover_dec.py` in
        /// `harness/codecs/aac/ps`, stages 1 and 5.
        lead: Cpx,
        /// Per section the fractional-delay phase exp(-i pi `FRACT[m]` f).
        frac: [Cpx; 3],
        /// Per section the coefficient a_m, tapered over the QMF bands (see [`layout`]).
        decay: [f32; 3],
    },
    Delay(usize),
}

#[derive(Clone, Copy, Debug)]
struct Band {
    qmf: usize,
    par: usize,
    /// Index of the band's filter among the split bands, if it is one.
    low: Option<usize>,
    image: bool,
    kind: Kind,
}

/// The hybrid bands of one configuration.
#[derive(Debug)]
pub(super) struct Layout {
    bands: Vec<Band>,
    /// Hybrid filter taps, d = -6..6 (tap d multiplies X\[t - d\]), per split band.
    taps: Vec<[Cpx; 13]>,
    /// QMF bands split by the hybrid filters.
    split: usize,
    /// Parameter bands.
    pub(super) pars: usize,
    /// Parameter bands that hold an image band.
    pub(super) image_pars: [bool; MAX_PAR],
}

/// A split band as the tables give it: (QMF band, split Q, sub-band mask, parameter band,
/// decorrelator frequency, image band).
type LowBand = (u8, u8, u16, u8, f64, bool);

fn cis(x: f64) -> Cpx {
    Cpx::new(x.cos() as f32, x.sin() as f32)
}

/// The hybrid bands of one configuration. Above the split bands the all-pass coefficients
/// taper as a_m (1 - `SLOPE` (k - split)) in QMF band k: observed behaviour, given by no
/// available text; probes d08, d10, d14 and d16 (slope, start at the first QMF band not
/// split, none in the split bands), rerun by `signal/dec/recover_dec.py` stages 1, 2 and 5.
fn layout(big: bool) -> Layout {
    use std::f64::consts::PI;
    let (low, qmf_par, split): (&[LowBand], &[(u8, u8)], usize) = if big {
        (&t::LOW_34, &t::QMF_PAR_34, 5)
    } else {
        (&t::LOW_20, &t::QMF_PAR_20, 3)
    };
    let all_pass = |f: f64, s: f64| Kind::AllPass {
        lead: cis(-PI * t::FRACT0 * f),
        frac: std::array::from_fn(|m| cis(-PI * t::FRACT[m] * f)),
        decay: std::array::from_fn(|m| (t::DECAY[m] * s) as f32),
    };
    let mut bands = Vec::with_capacity(MAX_HYBRID);
    let mut taps = Vec::with_capacity(MAX_LOW);
    let mut image_pars = [false; MAX_PAR];
    for (i, &(k, q, mask, par, f, image)) in low.iter().enumerate() {
        let g = match (big, q) {
            (false, 8) => &t::PROTO_20_Q8,
            (false, _) => &t::PROTO_20_Q4,
            (true, 12) => &t::PROTO_34_Q12,
            (true, 8) => &t::PROTO_34_Q8,
            (true, _) => &t::PROTO_34_Q4,
        };
        let qf = f64::from(q);
        taps.push(std::array::from_fn(|n| {
            let d = n as f64 - 6.0;
            let (mut re, mut im) = (0.0, 0.0);
            for m in (0..q).filter(|m| mask >> m & 1 != 0) {
                let w = (2.0 * f64::from(m) + 1.0) * PI / qf * d;
                re += w.cos();
                im += w.sin();
            }
            if mask.count_ones() == 2 {
                im = 0.0; // a mirror pair: real
            }
            let gn = g[(n as isize - 6).unsigned_abs()];
            Cpx::new((gn * re) as f32, (gn * im) as f32)
        }));
        image_pars[par as usize] |= image;
        bands.push(Band {
            qmf: k as usize,
            par: par as usize,
            low: Some(i),
            image,
            kind: all_pass(f, 1.0),
        });
    }
    let mut par = 0;
    for k in split..64 {
        if let Some(&(_, p)) = qmf_par.iter().find(|&&(k0, _)| k0 as usize == k) {
            par = p as usize;
        }
        let kind = if k >= t::SHORT_START {
            Kind::Delay(1)
        } else if k >= t::LONG_START {
            Kind::Delay(t::LONG_DELAY)
        } else {
            // the observed coefficient taper (see above)
            all_pass(k as f64 + 0.5, 1.0 - t::SLOPE * (k - split) as f64)
        };
        bands.push(Band {
            qmf: k,
            par,
            low: None,
            image: false,
            kind,
        });
    }
    Layout {
        bands,
        taps,
        split,
        pars: par + 1,
        image_pars,
    }
}

/// The layouts of the 20- and 34-band configurations.
pub(super) fn layouts() -> &'static [Layout; 2] {
    static L: OnceLock<[Layout; 2]> = OnceLock::new();
    L.get_or_init(|| [layout(false), layout(true)])
}

/// Per-slot mixing matrices of one frame, per parameter band: h11, h12, h21, h22 real parts,
/// then imaginary parts; `image` holds the imaginary parts for the image bands.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SlotMatrix {
    pub(super) re: [f32; 4],
    pub(super) im: [f32; 4],
    pub(super) image_im: [f32; 4],
}

/// Signal state and scratch of the synthesis.
#[derive(Debug)]
pub(super) struct Synth {
    /// The last 6 input slots of the QMF bands either configuration splits.
    history: [[Cpx; AHEAD]; MAX_SPLIT],
    /// Delay-line state per hybrid band: the lead delay and the three sections (2 + 3 + 4 +
    /// 5 slots) of an all-pass band, the newest slots of a delay band.
    lines: BudgetVec<[Cpx; 14]>,
    /// The band's delay lines hold nonzero values.
    busy: [bool; MAX_HYBRID],
    /// Ducking state per parameter band: peak, smoothed peak excess, smoothed power.
    duck: [[f32; 3]; MAX_PAR],
    /// Scratch: hybrid samples of the split bands, ducking gains per slot.
    low: BudgetVec<[Cpx; SLOTS]>,
    gain: BudgetVec<[f32; MAX_PAR]>,
    /// Scratch: the frame's per-slot matrices.
    pub(super) matrix: BudgetVec<[SlotMatrix; MAX_PAR]>,
    /// Some slot of the frame has complex matrices.
    pub(super) complex: bool,
}

impl Synth {
    pub(super) fn new(budget: &Budget) -> Result<Synth> {
        Ok(Synth {
            history: [[Cpx::default(); AHEAD]; MAX_SPLIT],
            lines: BudgetVec::try_filled(budget, [Cpx::default(); 14], MAX_HYBRID)?,
            busy: [false; MAX_HYBRID],
            duck: [[0.0; 3]; MAX_PAR],
            low: BudgetVec::try_filled(budget, [Cpx::default(); SLOTS], MAX_LOW)?,
            gain: BudgetVec::try_filled(budget, [0.0; MAX_PAR], SLOTS)?,
            matrix: BudgetVec::try_filled(budget, [SlotMatrix::default(); MAX_PAR], SLOTS)?,
            complex: false,
        })
    }

    /// Zero signal state, as at the first PS frame of a stream.
    pub(super) fn reset(&mut self) {
        self.history = [[Cpx::default(); AHEAD]; MAX_SPLIT];
        self.restart();
    }

    /// Zero decorrelator and ducking state, as at a change between 20 and 34 bands (the
    /// hybrid filter history is kept).
    pub(super) fn restart(&mut self) {
        self.lines.fill([Cpx::default(); 14]);
        self.busy = [false; MAX_HYBRID];
        self.duck = [[0.0; 3]; MAX_PAR];
    }

    /// Process one frame in configuration `big`: `x` (38 slots: 32 of the frame and 6
    /// look-ahead slots of the low band) becomes the left channel, `x_r` the right one.
    /// `self.matrix` holds the frame's matrices. Bands without input whose state is zero
    /// stay zero and are skipped.
    pub(super) fn run(&mut self, big: bool, x: &mut [[Cpx; 64]], x_r: &mut [[Cpx; 64]]) {
        let lay = &layouts()[usize::from(big)];
        let zero = Cpx::default();
        let top = (lay.split..64)
            .rev()
            .find(|&k| x[..SLOTS].iter().any(|r| r[k] != zero))
            .map_or(lay.split, |k| k + 1);
        let mut live = [false; MAX_HYBRID];
        for ((l, band), &busy) in live.iter_mut().zip(&lay.bands).zip(&self.busy) {
            *l = band.low.is_some() || band.qmf < top || busy;
        }
        self.analysis(lay, x);
        self.ducking(lay, x, &live);
        for row in x[..SLOTS].iter_mut().chain(x_r[..SLOTS].iter_mut()) {
            row[..lay.split].fill(zero);
        }
        let mut m = [zero; SLOTS];
        let mut d = [zero; SLOTS];
        for (j, band) in lay.bands.iter().enumerate() {
            if !live[j] {
                for row in &mut x_r[..SLOTS] {
                    row[band.qmf] = zero;
                }
                continue;
            }
            match band.low {
                Some(i) => m = self.low[i],
                None => {
                    for (v, row) in m.iter_mut().zip(x.iter()) {
                        *v = row[band.qmf];
                    }
                }
            }
            decorrelate(band.kind, &mut self.lines[j], &m, &mut d);
            self.busy[j] = self.lines[j].iter().any(|&v| v != zero);
            for (v, g) in d.iter_mut().zip(self.gain.iter()) {
                let g = g[band.par];
                v.re *= g;
                v.im *= g;
            }
            self.mix(band, &m, &d, x, x_r);
        }
    }

    /// Hybrid analysis of the split bands into `self.low`; keeps the input history.
    fn analysis(&mut self, lay: &Layout, x: &[[Cpx; 64]]) {
        let mut ext = [[Cpx::default(); SLOTS + 2 * AHEAD]; MAX_SPLIT];
        // the history of every band the hybrid filters can split, whatever the configuration
        for (k, e) in ext.iter_mut().enumerate() {
            e[..AHEAD].copy_from_slice(&self.history[k]);
            for (v, row) in e[AHEAD..].iter_mut().zip(x.iter()) {
                *v = row[k];
            }
            self.history[k].copy_from_slice(&e[SLOTS..SLOTS + AHEAD]);
        }
        for (band, out) in lay.bands.iter().zip(self.low.iter_mut()) {
            let Some(i) = band.low else { break };
            let f = &lay.taps[i];
            let e = &ext[band.qmf];
            let pair = f[6].im == 0.0 && f[5].im == 0.0;
            for (t, o) in out.iter_mut().enumerate() {
                // M[t] = sum_d f[d] X[t - d], X[t'] at e[t' + 6]
                let w = &e[t..t + 13];
                let mut acc = Cpx::default();
                if pair {
                    for (c, v) in f.iter().zip(w.iter().rev()) {
                        acc.re += c.re * v.re;
                        acc.im += c.re * v.im;
                    }
                } else {
                    for (c, v) in f.iter().zip(w.iter().rev()) {
                        let p = c.mul(*v);
                        acc.re += p.re;
                        acc.im += p.im;
                    }
                }
                *o = acc;
            }
        }
    }

    /// Ducking gains per slot and parameter band from the power of the input.
    ///
    /// Observed behaviour, given by no available text (Purnhagen section 4.2 names transient
    /// reduction in the decoder without a form). Per parameter band b and QMF slot t, with
    /// P the power of the PS input summed over b's decorrelator bands and the state zero at
    /// the first PS frame and after a restart:
    /// - K = max(a K', P), a decaying peak;
    /// - E = E' + c (P - E'), the smoothed power;
    /// - Q = Q' + c (K - P - Q'), the smoothed excess of the peak over the power;
    /// - g = E / (γ Q) when γ Q > E, else 1, multiplying the decorrelated signal of b's
    ///   bands in the same slot.
    ///
    /// a = exp(-4/15) (fit 0.7659282906 ± 4.7e-7; `PEAK_DECAY`), c = 1/4 (`SMOOTH`) and
    /// γ = 3/2 (`DUCK_LIMIT`). Probes duck-p01 to duck-p13 establish the form: gains per
    /// parameter band (p01, p04), fits of the candidates on tone envelopes (p02, p03, p05),
    /// the form against every stream (p06, p10, both configurations), its details (p07,
    /// p08, p11 to p13) and the constants (p09). Eleven other candidates are rejected, each
    /// by a probe (`harness/codecs/aac/ps/README.md`). Both derivations of this form
    /// remembered a reducer of this kind in FFmpeg with roughly these constants; neither
    /// seeded a fit with them, and the memory may have put the form among the candidates.
    /// The form rests on the probes.
    fn ducking(&mut self, lay: &Layout, x: &[[Cpx; 64]], live: &[bool; MAX_HYBRID]) {
        let peak_decay = t::PEAK_DECAY as f32;
        let smooth = t::SMOOTH as f32;
        let limit = t::DUCK_LIMIT as f32;
        // the power of every parameter band, summed band by band
        for row in self.gain.iter_mut() {
            row.fill(0.0);
        }
        for (band, _) in lay.bands.iter().zip(live).filter(|(_, l)| **l) {
            for (t, row) in self.gain.iter_mut().enumerate() {
                let v = match band.low {
                    Some(i) => self.low[i][t],
                    None => x[t][band.qmf],
                };
                row[band.par] += v.re * v.re + v.im * v.im;
            }
        }
        for row in self.gain.iter_mut() {
            for (g, s) in row.iter_mut().zip(self.duck.iter_mut()).take(lay.pars) {
                let p = *g;
                s[0] = p.max(peak_decay * s[0]);
                s[1] += smooth * (s[0] - p - s[1]);
                s[2] += smooth * (p - s[2]);
                *g = if limit * s[1] > s[2] {
                    s[2] / (limit * s[1])
                } else {
                    1.0
                };
            }
        }
    }

    /// L = h11 M + h12 D and R = h21 M + h22 D for one band, added into its QMF band.
    fn mix(
        &self,
        band: &Band,
        m: &[Cpx; SLOTS],
        d: &[Cpx; SLOTS],
        x: &mut [[Cpx; 64]],
        x_r: &mut [[Cpx; 64]],
    ) {
        let k = band.qmf;
        let add = band.low.is_some();
        for t in 0..SLOTS {
            let h = &self.matrix[t][band.par];
            let (m, d) = (m[t], d[t]);
            let (l, r) = if self.complex {
                let im = if band.image { &h.image_im } else { &h.im };
                let c = |i: usize| Cpx::new(h.re[i], im[i]);
                let l = c(0).mul(m);
                let l2 = c(1).mul(d);
                let r = c(2).mul(m);
                let r2 = c(3).mul(d);
                (
                    Cpx::new(l.re + l2.re, l.im + l2.im),
                    Cpx::new(r.re + r2.re, r.im + r2.im),
                )
            } else {
                let h = &h.re;
                (
                    Cpx::new(h[0] * m.re + h[1] * d.re, h[0] * m.im + h[1] * d.im),
                    Cpx::new(h[2] * m.re + h[3] * d.re, h[2] * m.im + h[3] * d.im),
                )
            };
            if add {
                x[t][k].re += l.re;
                x[t][k].im += l.im;
                x_r[t][k].re += r.re;
                x_r[t][k].im += r.im;
            } else {
                x[t][k] = l;
                x_r[t][k] = r;
            }
        }
    }
}

/// The linear decorrelator of one band over a frame, with its state in `line`.
fn decorrelate(kind: Kind, line: &mut [Cpx; 14], x: &[Cpx; SLOTS], y: &mut [Cpx; SLOTS]) {
    match kind {
        Kind::Delay(n) => {
            // line[..n] holds the last n inputs, oldest first
            let mut buf = [Cpx::default(); 14 + SLOTS];
            buf[..n].copy_from_slice(&line[..n]);
            buf[n..n + SLOTS].copy_from_slice(x);
            y.copy_from_slice(&buf[..SLOTS]);
            line[..n].copy_from_slice(&buf[SLOTS..SLOTS + n]);
        }
        Kind::AllPass { lead, frac, decay } => {
            // lead: y0[t] = lead x[t - 2]
            let mut cur = [Cpx::default(); SLOTS];
            let mut buf = [Cpx::default(); 5 + SLOTS];
            buf[..t::LEAD_DELAY].copy_from_slice(&line[..t::LEAD_DELAY]);
            buf[t::LEAD_DELAY..t::LEAD_DELAY + SLOTS].copy_from_slice(x);
            for (c, v) in cur.iter_mut().zip(&buf[..SLOTS]) {
                *c = lead.mul(*v);
            }
            line[..t::LEAD_DELAY].copy_from_slice(&buf[SLOTS..SLOTS + t::LEAD_DELAY]);
            // sections: w[t] = in[t] + a Q w[t - n]; out[t] = -a w[t] + Q w[t - n]
            let mut off = t::LEAD_DELAY;
            for ((&n, q), a) in t::SECTION_DELAY.iter().zip(frac).zip(decay) {
                let aq = Cpx::new(a * q.re, a * q.im);
                buf[..n].copy_from_slice(&line[off..off + n]);
                for i in 0..SLOTS {
                    let old = buf[i];
                    let fb = aq.mul(old);
                    let w = Cpx::new(cur[i].re + fb.re, cur[i].im + fb.im);
                    buf[n + i] = w;
                    let o = q.mul(old);
                    cur[i] = Cpx::new(o.re - a * w.re, o.im - a * w.im);
                }
                line[off..off + n].copy_from_slice(&buf[SLOTS..SLOTS + n]);
                off += n;
            }
            y.copy_from_slice(&cur);
        }
    }
}
