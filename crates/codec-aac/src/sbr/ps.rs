//! Parametric Stereo decoding (ISO/IEC 14496-3 subpart 8, 8.4 syntax / 8.6.4 decoding):
//! ps_data() parsing, hybrid QMF analysis/synthesis, all-pass decorrelation with transient
//! attenuation, and IID/ICC(/IPD/OPD) stereo mixing with per-envelope interpolation.

use crate::error::{DecodeError, Result};
use crate::fft::Cpx;
use crate::huffman::{Vlc, books};
use std::f64::consts::PI;
use std::sync::OnceLock;
use triergon_bitstream::CachedBitReader;
use triergon_codec::{Budget, BudgetVec};

const NUM_SLOTS: usize = 32;
/// Hybrid filter length and its group delay in QMF slots.
const HYB_LEN: usize = 13;
const HYB_DELAY: usize = 6;
const MAX_DELAY: usize = 14;
const AP_LINKS: usize = 3;

/// Number of hybrid bands, all-pass bands, 14-slot-delay limit, decay cutoff; [20-band, 34-band].
const NR_BANDS: [usize; 2] = [71, 91];
const NR_PAR_BANDS: [usize; 2] = [20, 34];
const NR_ALLPASS: [usize; 2] = [30, 50];
const SHORT_DELAY: [usize; 2] = [42, 62];
const DECAY_CUTOFF: [usize; 2] = [10, 32];
const DECAY_SLOPE: f32 = 0.05;
const NR_IPDOPD_BANDS: [usize; 2] = [11, 17];

/// iid / icc / ipd-opd parameter counts per mode (Table 8.?).
const NR_IIDICC_PAR: [usize; 6] = [10, 20, 34, 10, 20, 34];
const NR_IPDOPD_PAR: [usize; 6] = [5, 11, 17, 5, 11, 17];
const NUM_ENV_TAB: [[usize; 4]; 2] = [[0, 1, 2, 4], [1, 2, 3, 4]];

/// IID quantisation (dB), ISO/IEC 14496-3 Tables 8.25 (default) and 8.26 (fine).
const IID_COARSE: [f64; 15] = [
    -25.0, -18.0, -14.0, -10.0, -7.0, -4.0, -2.0, 0.0, 2.0, 4.0, 7.0, 10.0, 14.0, 18.0, 25.0,
];
const IID_FINE: [f64; 31] = [
    -50.0, -45.0, -40.0, -35.0, -30.0, -25.0, -22.0, -19.0, -16.0, -13.0, -10.0, -8.0, -6.0, -4.0,
    -2.0, 0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 13.0, 16.0, 19.0, 22.0, 25.0, 30.0, 35.0, 40.0, 45.0,
    50.0,
];
/// ICC quantisation, ISO/IEC 14496-3 Table 8.27.
const ICC: [f64; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];

/// Hybrid prototype filters (first 7 taps of the symmetric 13-tap filters, 8.6.4.3).
/// 8-band (20-band configuration, QMF band 0), 3GPP TS 26.405 Table 1 column 1.
const G0_Q8: [f64; 7] = [
    0.00746082949812,
    0.02270420949825,
    0.04546865930473,
    0.07266113929591,
    0.09885108575264,
    0.11793710567217,
    0.125,
];
/// 2-band real filter (20-band configuration, QMF bands 1 and 2).
const G1_Q2: [f64; 7] = [
    0.0,
    0.01899487526049,
    0.0,
    -0.07293139167538,
    0.0,
    0.30596630545168,
    0.5,
];
/// 12-band (34-band configuration, QMF band 0).
const G0_Q12: [f64; 7] = [
    0.04081179924692,
    0.03812810994926,
    0.05144908135699,
    0.06399831151592,
    0.07428313801106,
    0.08100347892914,
    0.08333333333333,
];
/// 8-band (34-band configuration, QMF band 1).
const G1_Q8: [f64; 7] = [
    0.01565675600122,
    0.03752716391991,
    0.05417891378782,
    0.08417044116767,
    0.10307344158036,
    0.12222452249753,
    0.125,
];
/// 4-band (34-band configuration, QMF bands 2..4).
const G2_Q4: [f64; 7] = [
    -0.05908211155639,
    -0.04871498374946,
    0.0,
    0.07778723915851,
    0.16486303567403,
    0.23279856662996,
    0.25,
];

/// Hybrid band -> parameter band, 20-band configuration (Table 8.48).
const K_TO_I_20: [u8; 71] = [
    1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 14, 15, 15, 15, 16, 16, 16, 16, 17, 17,
    17, 17, 17, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19, 19,
    19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19, 19,
];
/// Hybrid band -> parameter band, 34-band configuration (Table 8.49, recalled). Hybrid bands
/// 0..11 are the 12 sub-bands of QMF band 0 in filter order q = 0..11, 12..19 the 8 of QMF
/// band 1, 20..31 the 4 of QMF bands 2, 3, 4; this order is the one that makes F_CENTER_34
/// equal to each filter's centre frequency (see there).
const K_TO_I_34: [u8; 91] = [
    0, 1, 2, 3, 4, 5, 6, 6, 7, 2, 1, 0, 10, 10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 9, 14, 11, 12, 13,
    14, 15, 16, 13, 16, 17, 18, 19, 20, 21, 22, 22, 23, 23, 24, 24, 25, 25, 26, 26, 27, 27, 27, 28,
    28, 28, 29, 29, 29, 30, 30, 30, 31, 31, 31, 31, 32, 32, 32, 32, 33, 33, 33, 33, 33, 33, 33, 33,
    33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33, 33,
];
/// Centre frequencies of the hybrid sub-subbands (in QMF band units x8 for 20 bands,
/// x24 for 34 bands); QMF bands beyond use k - 6.5 (20) / k - 26.5 (34). Recalled, and
/// consistent with the filter bank: QMF band p (even p: slot-rate frequency c maps to p + 2c,
/// odd p: to p + 1 + 2c, modulo 2) filtered by sub-band filter q of Q bands, c = (q + 1/2)/Q.
const F_CENTER_20: [i32; 10] = [-3, -1, 1, 3, 5, 7, 10, 14, 18, 22];
const F_CENTER_34: [i32; 32] = [
    2, 6, 10, 14, 18, 22, 26, 30, 34, -10, -6, -2, 51, 57, 15, 21, 27, 33, 39, 45, 54, 66, 78, 42,
    102, 66, 78, 90, 102, 114, 126, 90,
];
const FRACT_DELAY_GAIN: f64 = 0.39;
const FRACT_DELAY_LINKS: [f64; 3] = [0.43, 0.75, 0.347];
#[allow(clippy::excessive_precision)] // printed precision of the source
const AP_COEF: [f32; 3] = [0.65143905753106, 0.56471812200776, 0.48954165955695];
const LINK_DELAY: [usize; 3] = [3, 4, 5];

struct Tables {
    /// hybrid analysis filters [config][filter set] as (q, tap) complex taps
    f20_0_8: [[Cpx; 13]; 8],
    f34_0_12: [[Cpx; 13]; 12],
    f34_1_8: [[Cpx; 13]; 8],
    f34_2_4: [[Cpx; 13]; 4],
    g1_q2: [f32; 13],
    phi_fract: [Vec<Cpx>; 2],
    q_fract: [Vec<[Cpx; 3]>; 2],
    /// mixing matrices [iid index (0..46: coarse then fine)][icc][h11,h12,h21,h22], procedure A / B
    ha: Vec<[[f32; 4]; 8]>,
    hb: Vec<[[f32; 4]; 8]>,
    /// smoothed phase table for ipd/opd (re, im) indexed by (hist2*64 + hist1*8 + cur)
    pd_smooth: Vec<Cpx>,
}

fn complex_filters<const Q: usize>(proto: &[f64; 7]) -> [[Cpx; 13]; Q] {
    let mut f = [[Cpx::default(); 13]; Q];
    for (q, fq) in f.iter_mut().enumerate() {
        for n in 0..13 {
            let g = if n <= 6 { proto[n] } else { proto[12 - n] };
            let th = 2.0 * PI * (q as f64 + 0.5) * (n as f64 - 6.0) / Q as f64;
            fq[n] = Cpx::new((g * th.cos()) as f32, (-g * th.sin()) as f32);
        }
    }
    f
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut g1_q2 = [0f32; 13];
        for n in 0..13 {
            g1_q2[n] = if n <= 6 { G1_Q2[n] } else { G1_Q2[12 - n] } as f32;
        }
        let mut phi_fract = [Vec::new(), Vec::new()];
        let mut q_fract = [Vec::new(), Vec::new()];
        for cfg in 0..2 {
            for k in 0..NR_ALLPASS[cfg] {
                let fc = if cfg == 0 {
                    if k < F_CENTER_20.len() {
                        F_CENTER_20[k] as f64 * 0.125
                    } else {
                        k as f64 - 6.5
                    }
                } else if k < F_CENTER_34.len() {
                    F_CENTER_34[k] as f64 / 24.0
                } else {
                    k as f64 - 26.5
                };
                let th = -PI * FRACT_DELAY_GAIN * fc;
                phi_fract[cfg].push(Cpx::new(th.cos() as f32, th.sin() as f32));
                let mut qf = [Cpx::default(); 3];
                for m in 0..3 {
                    let th = -PI * FRACT_DELAY_LINKS[m] * fc;
                    qf[m] = Cpx::new(th.cos() as f32, th.sin() as f32);
                }
                q_fract[cfg].push(qf);
            }
        }
        // mixing tables
        let mut ha = Vec::new();
        let mut hb = Vec::new();
        let iids: Vec<f64> = IID_COARSE.iter().chain(IID_FINE.iter()).copied().collect();
        for &iid in &iids {
            let c = 10f64.powf(iid / 20.0);
            let c1 = (2.0 / (1.0 + c * c)).sqrt();
            let c2 = (2.0 * c * c / (1.0 + c * c)).sqrt();
            let mut ra = [[0f32; 4]; 8];
            let mut rb = [[0f32; 4]; 8];
            for (j, &rho) in ICC.iter().enumerate() {
                // procedure A (8.6.4.6.2)
                let alpha = 0.5 * rho.clamp(-1.0, 1.0).acos();
                let beta = alpha * (c1 - c2) / 2f64.sqrt();
                ra[j] = [
                    (c2 * (beta + alpha).cos()) as f32,
                    (c1 * (beta - alpha).cos()) as f32,
                    (c2 * (beta + alpha).sin()) as f32,
                    (c1 * (beta - alpha).sin()) as f32,
                ];
                // procedure B (8.6.4.6.3)
                // (unverified: no vector uses icc_mode > 2)
                let alpha_b = if (c - 1.0).abs() < 1e-12 {
                    if rho < 0.0 { -PI / 4.0 } else { PI / 4.0 }
                } else {
                    0.5 * (2.0 * c * rho / (c * c - 1.0)).atan()
                };
                let alpha_b = if c < 1.0 { alpha_b + PI / 2.0 } else { alpha_b };
                let mu = c + 1.0 / c;
                let mu = 1.0 + (4.0 * rho * rho - 4.0) / (mu * mu);
                let gamma = ((1.0 - mu.max(0.0).sqrt()) / (1.0 + mu.max(0.0).sqrt()))
                    .sqrt()
                    .atan();
                let s2 = 2f64.sqrt();
                rb[j] = [
                    (s2 * alpha_b.cos() * gamma.cos()) as f32,
                    (s2 * alpha_b.sin() * gamma.cos()) as f32,
                    (-s2 * alpha_b.sin() * gamma.sin()) as f32,
                    (s2 * alpha_b.cos() * gamma.sin()) as f32,
                ];
            }
            ha.push(ra);
            hb.push(rb);
        }
        // ipd/opd phase smoothing: angle of 0.25 e^{i p2} + 0.5 e^{i p1} + e^{i p0}
        let mut pd_smooth = Vec::with_capacity(512);
        for idx in 0..512usize {
            let p2 = ((idx >> 6) & 7) as f64 * PI / 4.0;
            let p1 = ((idx >> 3) & 7) as f64 * PI / 4.0;
            let p0 = (idx & 7) as f64 * PI / 4.0;
            let re = 0.25 * p2.cos() + 0.5 * p1.cos() + p0.cos();
            let im = 0.25 * p2.sin() + 0.5 * p1.sin() + p0.sin();
            let a = im.atan2(re);
            pd_smooth.push(Cpx::new(a.cos() as f32, a.sin() as f32));
        }
        Tables {
            f20_0_8: complex_filters::<8>(&G0_Q8),
            f34_0_12: complex_filters::<12>(&G0_Q12),
            f34_1_8: complex_filters::<8>(&G1_Q8),
            f34_2_4: complex_filters::<4>(&G2_Q4),
            g1_q2,
            phi_fract,
            q_fract,
            ha,
            hb,
            pd_smooth,
        }
    })
}

/// Parameters and filter state that a reset returns to `State::INITIAL`.
#[derive(Clone, Copy, Debug)]
struct State {
    enable_ext: bool,
    nr_iid_par: usize,
    nr_icc_par: usize,
    nr_ipdopd_par: usize,
    iid_fine: bool,
    enable_ipdopd: bool,
    is34: bool,
    is34_prev: bool,
    // frame data
    num_env: usize,
    num_env_prev: usize,
    border: [i32; 6],
    iid: [[i8; 34]; 6],
    icc: [[i8; 34]; 6],
    ipd: [[i8; 17]; 6],
    opd: [[i8; 17]; 6],
    ipd_hist: [u8; 17],
    opd_hist: [u8; 17],
    // signal state
    peak_decay: [f32; 34],
    power_smooth: [f32; 34],
    peak_decay_diff_smooth: [f32; 34],
    /// mixing coefficients at the end of the previous frame [re/im][4][par band]
    h_prev: [[[f32; 34]; 4]; 2],
}

impl State {
    const INITIAL: State = State {
        enable_ext: false,
        nr_iid_par: 10,
        nr_icc_par: 10,
        nr_ipdopd_par: 5,
        iid_fine: false,
        enable_ipdopd: false,
        is34: false,
        is34_prev: false,
        num_env: 0,
        num_env_prev: 0,
        border: [0; 6],
        iid: [[0; 34]; 6],
        icc: [[0; 34]; 6],
        ipd: [[0; 17]; 6],
        opd: [[0; 17]; 6],
        ipd_hist: [0; 17],
        opd_hist: [0; 17],
        peak_decay: [0.0; 34],
        power_smooth: [0.0; 34],
        peak_decay_diff_smooth: [0.0; 34],
        h_prev: [[[0.0; 34]; 4]; 2],
    };
}

#[derive(Debug)]
pub(crate) struct Ps {
    // header state, kept across a reset
    enable_iid: bool,
    enable_icc: bool,
    iid_mode: u8,
    icc_mode: u8,
    header_seen: bool,
    st: State,
    /// QMF input history for the hybrid filters (bands 0..4, 12 slots) + delay for bands >= 3/5
    qmf_hist: BudgetVec<[Cpx; 64]>,
    /// mono hybrid matrix for the frame: s[k][n]
    s: BudgetVec<[Cpx; NUM_SLOTS]>,
    /// decorrelated output
    d: BudgetVec<[Cpx; NUM_SLOTS]>,
    delay: BudgetVec<[Cpx; MAX_DELAY + NUM_SLOTS]>,
    ap_delay: BudgetVec<[[Cpx; NUM_SLOTS + 5]; AP_LINKS]>,
    out_l: BudgetVec<[Cpx; NUM_SLOTS]>,
    out_r: BudgetVec<[Cpx; NUM_SLOTS]>,
}

impl Ps {
    pub(crate) fn new(budget: &Budget) -> Result<Ps> {
        let zero = [Cpx::default(); NUM_SLOTS];
        Ok(Ps {
            enable_iid: false,
            enable_icc: false,
            iid_mode: 0,
            icc_mode: 0,
            header_seen: false,
            st: State::INITIAL,
            qmf_hist: BudgetVec::try_filled(
                budget,
                [Cpx::default(); 64],
                HYB_DELAY + NUM_SLOTS + HYB_DELAY,
            )?,
            s: BudgetVec::try_filled(budget, zero, 91)?,
            d: BudgetVec::try_filled(budget, zero, 91)?,
            delay: BudgetVec::try_filled(budget, [Cpx::default(); MAX_DELAY + NUM_SLOTS], 91)?,
            ap_delay: BudgetVec::try_filled(
                budget,
                [[Cpx::default(); NUM_SLOTS + 5]; AP_LINKS],
                50,
            )?,
            out_l: BudgetVec::try_filled(budget, zero, 91)?,
            out_r: BudgetVec::try_filled(budget, zero, 91)?,
        })
    }

    /// Return to the state of `new` in place, header included.
    pub(crate) fn renew(&mut self) {
        self.reset();
        let Ps {
            enable_iid,
            enable_icc,
            iid_mode,
            icc_mode,
            header_seen,
            st: _,
            qmf_hist: _,
            s: _,
            d: _,
            delay: _,
            ap_delay: _,
            out_l: _,
            out_r: _,
        } = self; // the fields set to `_` are reset's
        *enable_iid = false;
        *enable_icc = false;
        *iid_mode = 0;
        *icc_mode = 0;
        *header_seen = false;
    }

    /// Return to the state of `new`, keeping the header.
    pub(crate) fn reset(&mut self) {
        self.st = State::INITIAL;
        for buffer in [&mut self.s, &mut self.d, &mut self.out_l, &mut self.out_r] {
            buffer.fill([Cpx::default(); NUM_SLOTS]);
        }
        self.qmf_hist.fill([Cpx::default(); 64]);
        self.clear_decorrelator();
    }

    /// Clear the decorrelator delay lines.
    fn clear_decorrelator(&mut self) {
        self.delay.fill([Cpx::default(); MAX_DELAY + NUM_SLOTS]);
        self.ap_delay
            .fill([[Cpx::default(); NUM_SLOTS + 5]; AP_LINKS]);
    }

    /// Switch between the 20- and 34-band configurations: the decorrelator state is
    /// cleared, the hybrid input history (QMF domain) is kept, and the interpolation start
    /// values of the mixing matrices are mapped to the new parameter-band resolution.
    fn switch_config(&mut self, to34: bool) {
        self.clear_decorrelator();
        self.st.peak_decay = [0.0; 34];
        self.st.power_smooth = [0.0; 34];
        self.st.peak_decay_diff_smooth = [0.0; 34];
        for p in &mut self.st.h_prev {
            for c in p.iter_mut() {
                *c = if to34 {
                    map_20_to_34(c)
                } else {
                    map_34_to_20(c)
                };
            }
        }
    }

    /// ps_data(); `bits` is the number of bits available in the SBR extension.
    pub(crate) fn parse(&mut self, br: &mut CachedBitReader<'_>, bits: usize) -> Result<usize> {
        let start = br.position();
        let bk = books();
        if br.bit() {
            self.header_seen = true;
            self.enable_iid = br.bit();
            if self.enable_iid {
                self.iid_mode = br.read(3) as u8;
                if self.iid_mode > 5 {
                    return Err(DecodeError::InvalidData("PS iid_mode"));
                }
                self.st.nr_iid_par = NR_IIDICC_PAR[self.iid_mode as usize];
                self.st.nr_ipdopd_par = NR_IPDOPD_PAR[self.iid_mode as usize];
                self.st.iid_fine = self.iid_mode > 2;
            }
            self.enable_icc = br.bit();
            if self.enable_icc {
                self.icc_mode = br.read(3) as u8;
                if self.icc_mode > 5 {
                    return Err(DecodeError::InvalidData("PS icc_mode"));
                }
                self.st.nr_icc_par = NR_IIDICC_PAR[self.icc_mode as usize];
            }
            self.st.enable_ext = br.bit();
        }
        let frame_class = br.bit() as usize;
        self.st.num_env_prev = self.st.num_env;
        let num_env = NUM_ENV_TAB[frame_class][br.read(2) as usize];
        self.st.border[0] = -1;
        for (e, b) in self
            .st
            .border
            .iter_mut()
            .enumerate()
            .take(num_env + 1)
            .skip(1)
        {
            *b = if frame_class == 1 {
                br.read(5) as i32
            } else {
                ((e * NUM_SLOTS) / num_env) as i32 - 1
            };
        }
        // keep the last envelope of the previous frame as reference [0]
        let last = self.st.num_env_prev;
        self.st.iid[0] = self.st.iid[last];
        self.st.icc[0] = self.st.icc[last];
        self.st.ipd[0] = self.st.ipd[last];
        self.st.opd[0] = self.st.opd[last];
        if self.enable_iid {
            let (t_df, t_dt, off, lim) = if self.st.iid_fine {
                (&bk.ps[0], &bk.ps[1], 30, 15)
            } else {
                (&bk.ps[2], &bk.ps[3], 14, 7)
            };
            for e in 1..=num_env {
                let dt = br.bit();
                read_par(
                    br,
                    if dt { t_dt } else { t_df },
                    dt,
                    off,
                    self.st.nr_iid_par,
                    &mut self.st.iid,
                    e,
                )?;
                if self.st.iid[e][..self.st.nr_iid_par]
                    .iter()
                    .any(|&v| v.abs() as i32 > lim)
                {
                    return Err(DecodeError::InvalidData("PS iid out of range"));
                }
            }
        } else {
            self.st.iid[1..=num_env].fill([0; 34]);
        }
        if self.enable_icc {
            for e in 1..=num_env {
                let dt = br.bit();
                read_par(
                    br,
                    if dt { &bk.ps[5] } else { &bk.ps[4] },
                    dt,
                    7,
                    self.st.nr_icc_par,
                    &mut self.st.icc,
                    e,
                )?;
                if self.st.icc[e][..self.st.nr_icc_par]
                    .iter()
                    .any(|v| !(0..=7).contains(v))
                {
                    return Err(DecodeError::InvalidData("PS icc out of range"));
                }
            }
        } else {
            self.st.icc[1..=num_env].fill([0; 34]);
        }
        self.st.enable_ipdopd = false;
        if self.st.enable_ext {
            let mut cnt = br.read(4) as usize;
            if cnt == 15 {
                cnt += br.read(8) as usize;
            }
            let mut left = cnt as isize * 8;
            while left > 7 {
                let id = br.read(2);
                left -= 2;
                let p0 = br.position();
                if id == 0 {
                    self.st.enable_ipdopd = br.bit();
                    if self.st.enable_ipdopd {
                        for e in 1..=num_env {
                            let dt = br.bit();
                            let (tdf, tdt) = (&bk.ps[6], &bk.ps[7]);
                            read_ipd(
                                br,
                                if dt { tdt } else { tdf },
                                dt,
                                self.st.nr_ipdopd_par,
                                &mut self.st.ipd,
                                e,
                            )?;
                            let dt = br.bit();
                            let (tdf, tdt) = (&bk.ps[8], &bk.ps[9]);
                            read_ipd(
                                br,
                                if dt { tdt } else { tdf },
                                dt,
                                self.st.nr_ipdopd_par,
                                &mut self.st.opd,
                                e,
                            )?;
                        }
                    }
                    br.skip(1); // reserved_ps
                } else {
                    br.skip(left as usize);
                }
                left -= (br.position() - p0) as isize;
            }
            if left > 0 {
                br.skip(left as usize);
            }
        }
        if !self.st.enable_ipdopd {
            for e in 1..=num_env {
                self.st.ipd[e] = [0; 17];
                self.st.opd[e] = [0; 17];
            }
        }
        // no new envelope: reuse the previous parameters for the whole frame
        let mut num_env = num_env;
        if num_env == 0 {
            num_env = 1;
            self.st.border[1] = NUM_SLOTS as i32 - 1;
            self.st.iid[1] = self.st.iid[0];
            self.st.icc[1] = self.st.icc[0];
            self.st.ipd[1] = self.st.ipd[0];
            self.st.opd[1] = self.st.opd[0];
        }
        // extend the last envelope to the end of the frame
        if self.st.border[num_env] < NUM_SLOTS as i32 - 1 {
            num_env += 1;
            self.st.border[num_env] = NUM_SLOTS as i32 - 1;
            self.st.iid[num_env] = self.st.iid[num_env - 1];
            self.st.icc[num_env] = self.st.icc[num_env - 1];
            self.st.ipd[num_env] = self.st.ipd[num_env - 1];
            self.st.opd[num_env] = self.st.opd[num_env - 1];
        }
        for e in 1..=num_env {
            if self.st.border[e] <= self.st.border[e - 1] || self.st.border[e] >= NUM_SLOTS as i32 {
                return Err(DecodeError::InvalidData("PS envelope borders"));
            }
        }
        self.st.num_env = num_env;
        self.st.is34_prev = self.st.is34;
        if self.enable_iid || self.enable_icc {
            self.st.is34 = (self.enable_iid && self.st.nr_iid_par == 34)
                || (self.enable_icc && self.st.nr_icc_par == 34);
        }
        let used = br.position() - start;
        if used > bits {
            return Err(DecodeError::InvalidData("PS data overrun"));
        }
        Ok(used)
    }

    /// Apply PS to one frame: `l` holds the mono QMF matrix [slot][band] on input; on output
    /// `l` and `r` hold the left / right QMF matrices.
    pub(crate) fn apply(&mut self, l: &mut [[Cpx; 64]], r: &mut [[Cpx; 64]], _ok: bool) {
        let cfg = self.st.is34 as usize;
        if self.st.is34 != self.st.is34_prev {
            self.switch_config(self.st.is34);
            self.st.is34_prev = self.st.is34;
        }
        self.hybrid_analysis(l, cfg);
        self.decorrelate(cfg);
        self.stereo(cfg);
        hybrid_synthesis(&self.out_l, l, cfg);
        hybrid_synthesis(&self.out_r, r, cfg);
    }

    /// `x` holds 38 QMF slots: the 32 of this frame plus 6 look-ahead slots of the low band,
    /// so the 13-tap hybrid filters (centre tap 6) add no delay.
    fn hybrid_analysis(&mut self, x: &[[Cpx; 64]], cfg: usize) {
        let t = tables();
        // h[0..6] = last 6 slots of the previous frame, h[6..44] = this frame incl. look-ahead
        let h = &mut self.qmf_hist;
        h.copy_within(NUM_SLOTS..NUM_SLOTS + HYB_DELAY, 0);
        h[HYB_DELAY..HYB_DELAY + NUM_SLOTS + HYB_DELAY]
            .copy_from_slice(&x[..NUM_SLOTS + HYB_DELAY]);
        let s = &mut self.s;
        // output slot n uses input slots n .. n + 12 of the history buffer (delay 6)
        let filt = |band: usize, n: usize, f: &[Cpx; 13]| -> Cpx {
            let mut acc = Cpx::default();
            for j in 0..HYB_LEN {
                let v = h[n + j][band];
                acc.re += v.re * f[j].re - v.im * f[j].im;
                acc.im += v.re * f[j].im + v.im * f[j].re;
            }
            acc
        };
        if cfg == 0 {
            for n in 0..NUM_SLOTS {
                let mut q8 = [Cpx::default(); 8];
                for (q, f) in t.f20_0_8.iter().enumerate() {
                    q8[q] = filt(0, n, f);
                }
                s[0][n] = q8[6];
                s[1][n] = q8[7];
                s[2][n] = q8[0];
                s[3][n] = q8[1];
                s[4][n] = Cpx::new(q8[2].re + q8[5].re, q8[2].im + q8[5].im);
                s[5][n] = Cpx::new(q8[3].re + q8[4].re, q8[3].im + q8[4].im);
                // 2-band real filters for QMF bands 1 (spectrally reversed) and 2
                for (band, base, rev) in [(1usize, 6usize, true), (2, 8, false)] {
                    let mut lo = Cpx::default();
                    let mut hi = Cpx::default();
                    for j in 0..HYB_LEN {
                        let v = h[n + j][band];
                        let g = t.g1_q2[j];
                        // cos(pi (j - 6)) alternates sign for the high band
                        let sgn = if (j as isize - 6).rem_euclid(2) == 0 {
                            1.0
                        } else {
                            -1.0
                        };
                        lo.re += g * v.re;
                        lo.im += g * v.im;
                        hi.re += g * sgn * v.re;
                        hi.im += g * sgn * v.im;
                    }
                    let (a, b) = if rev { (hi, lo) } else { (lo, hi) };
                    s[base][n] = a;
                    s[base + 1][n] = b;
                }
                for k in 3..64 {
                    s[k + 7][n] = x[n][k];
                }
            }
        } else {
            for n in 0..NUM_SLOTS {
                let mut q12 = [Cpx::default(); 12];
                for (q, f) in t.f34_0_12.iter().enumerate() {
                    q12[q] = filt(0, n, f);
                }
                // the 12 sub-bands of QMF band 0 in filter order (see K_TO_I_34)
                for q in 0..12 {
                    s[q][n] = q12[q];
                }
                let mut q8 = [Cpx::default(); 8];
                for (q, f) in t.f34_1_8.iter().enumerate() {
                    q8[q] = filt(1, n, f);
                }
                for q in 0..8 {
                    s[12 + q][n] = q8[q];
                }
                for b in 0..3 {
                    for q in 0..4 {
                        s[20 + 4 * b + q][n] = filt(2 + b, n, &t.f34_2_4[q]);
                    }
                }
                for k in 5..64 {
                    s[k + 27][n] = x[n][k];
                }
            }
        }
    }

    fn decorrelate(&mut self, cfg: usize) {
        let t = tables();
        let k_to_i: &[u8] = if cfg == 1 { &K_TO_I_34 } else { &K_TO_I_20 };
        let nb = NR_BANDS[cfg];
        let npar = NR_PAR_BANDS[cfg];
        let mut power = [[0f32; NUM_SLOTS]; 34];
        for k in 0..nb {
            let i = k_to_i[k] as usize;
            for n in 0..NUM_SLOTS {
                let v = self.s[k][n];
                power[i][n] += v.re * v.re + v.im * v.im;
            }
        }
        // transient detection and gain (8.6.4.5.2)
        #[allow(clippy::excessive_precision)] // printed precision of the source
        const PEAK_DECAY: f32 = 0.76592833836465;
        const TRANSIENT_IMPACT: f32 = 1.5;
        const A_SMOOTH: f32 = 0.25;
        let mut gain = [[1f32; NUM_SLOTS]; 34];
        for i in 0..npar {
            for n in 0..NUM_SLOTS {
                let decayed = PEAK_DECAY * self.st.peak_decay[i];
                self.st.peak_decay[i] = decayed.max(power[i][n]);
                self.st.power_smooth[i] += A_SMOOTH * (power[i][n] - self.st.power_smooth[i]);
                self.st.peak_decay_diff_smooth[i] += A_SMOOTH
                    * (self.st.peak_decay[i] - power[i][n] - self.st.peak_decay_diff_smooth[i]);
                let denom = TRANSIENT_IMPACT * self.st.peak_decay_diff_smooth[i];
                gain[i][n] = if denom > self.st.power_smooth[i] {
                    self.st.power_smooth[i] / denom
                } else {
                    1.0
                };
            }
        }
        for k in 0..nb {
            let i = k_to_i[k] as usize;
            let dl = &mut self.delay[k];
            dl.copy_within(NUM_SLOTS..NUM_SLOTS + MAX_DELAY, 0);
            dl[MAX_DELAY..].copy_from_slice(&self.s[k]);
            if k < NR_ALLPASS[cfg] {
                let gd =
                    (1.0 - DECAY_SLOPE * (k as f32 - DECAY_CUTOFF[cfg] as f32)).clamp(0.0, 1.0);
                let ag = [AP_COEF[0] * gd, AP_COEF[1] * gd, AP_COEF[2] * gd];
                let phi = t.phi_fract[cfg][k];
                let qf = t.q_fract[cfg][k];
                let ap = &mut self.ap_delay[k];
                for link in ap.iter_mut() {
                    link.copy_within(NUM_SLOTS..NUM_SLOTS + 5, 0);
                }
                for n in 0..NUM_SLOTS {
                    let x = dl[MAX_DELAY - 2 + n];
                    let mut v = x.mul(phi);
                    for m in 0..AP_LINKS {
                        let ld = ap[m][n + 5 - LINK_DELAY[m]];
                        let a = Cpx::new(ag[m] * v.re, ag[m] * v.im);
                        let w = ld.mul(qf[m]);
                        let out = Cpx::new(w.re - a.re, w.im - a.im);
                        ap[m][n + 5] = Cpx::new(v.re + ag[m] * out.re, v.im + ag[m] * out.im);
                        v = out;
                    }
                    let g = gain[i][n];
                    self.d[k][n] = Cpx::new(g * v.re, g * v.im);
                }
            } else {
                let dly = if k < SHORT_DELAY[cfg] { 14 } else { 1 };
                for n in 0..NUM_SLOTS {
                    let v = dl[MAX_DELAY - dly + n];
                    let g = gain[i][n];
                    self.d[k][n] = Cpx::new(g * v.re, g * v.im);
                }
            }
        }
    }

    /// Stereo mixing (8.6.4.6) into the left / right hybrid matrices `out_l`, `out_r`.
    fn stereo(&mut self, cfg: usize) {
        let out_l: &mut [[Cpx; NUM_SLOTS]] = &mut self.out_l;
        let out_r: &mut [[Cpx; NUM_SLOTS]] = &mut self.out_r;
        let t = tables();
        let nb = NR_BANDS[cfg];
        let npar = NR_PAR_BANDS[cfg];
        let k_to_i: &[u8] = if cfg == 1 { &K_TO_I_34 } else { &K_TO_I_20 };
        let mixing_b = self.icc_mode > 2;
        let lut = if mixing_b { &t.hb } else { &t.ha };
        let iid_base = if self.st.iid_fine { 15 } else { 0 };
        let iid_off = if self.st.iid_fine { 15 } else { 7 };
        let ipdopd = self.st.enable_ipdopd;
        let mut h_start = self.st.h_prev;
        for e in 1..=self.st.num_env {
            // map the parameters to the processing resolution
            let iid_m = map_par(&self.st.iid[e], self.st.nr_iid_par, npar);
            let icc_m = map_par(&self.st.icc[e], self.st.nr_icc_par, npar);
            let ipd_m = map_pd(&self.st.ipd[e], self.st.nr_ipdopd_par, NR_IPDOPD_BANDS[cfg]);
            let opd_m = map_pd(&self.st.opd[e], self.st.nr_ipdopd_par, NR_IPDOPD_BANDS[cfg]);
            let mut h_end = [[[0f32; 34]; 4]; 2];
            for b in 0..npar {
                let iid = (iid_m[b] as i32 + iid_off) as usize + iid_base;
                let icc = icc_m[b].clamp(0, 7) as usize;
                let hh = lut[iid.min(45)][icc];
                let mut h = [hh[0], hh[1], hh[2], hh[3]];
                let mut hi = [0f32; 4];
                if ipdopd && b < NR_IPDOPD_BANDS[cfg] {
                    let oi = (self.st.opd_hist[b] as usize) * 8 + (opd_m[b] as usize & 7);
                    let ii = (self.st.ipd_hist[b] as usize) * 8 + (ipd_m[b] as usize & 7);
                    let op = t.pd_smooth[oi];
                    let ip = t.pd_smooth[ii];
                    self.st.opd_hist[b] = (oi & 0x3f) as u8;
                    self.st.ipd_hist[b] = (ii & 0x3f) as u8;
                    // phase of the right channel: opd - ipd
                    let adj =
                        Cpx::new(op.re * ip.re + op.im * ip.im, op.im * ip.re - op.re * ip.im);
                    hi = [h[0] * op.im, h[1] * adj.im, h[2] * op.im, h[3] * adj.im];
                    h = [h[0] * op.re, h[1] * adj.re, h[2] * op.re, h[3] * adj.re];
                }
                for c in 0..4 {
                    h_end[0][c][b] = h[c];
                    h_end[1][c][b] = hi[c];
                }
            }
            let start = self.st.border[e - 1];
            let stop = self.st.border[e];
            let width = 1.0 / ((stop - start).max(1) as f32);
            for k in 0..nb {
                let b = k_to_i[k] as usize;
                let mut h = [[0f32; 4]; 2];
                let mut step = [[0f32; 4]; 2];
                for p in 0..2 {
                    for c in 0..4 {
                        h[p][c] = h_start[p][c][b];
                        step[p][c] = (h_end[p][c][b] - h[p][c]) * width;
                    }
                }
                if ipdopd && ((cfg == 1 && (9..=13).contains(&k)) || (cfg == 0 && k <= 1)) {
                    // negative-frequency hybrid bands: conjugate phase rotation
                    for c in 0..4 {
                        h[1][c] = -h[1][c];
                        step[1][c] = -step[1][c];
                    }
                }
                for n in (start + 1) as usize..=(stop as usize) {
                    for c in 0..4 {
                        h[0][c] += step[0][c];
                        h[1][c] += step[1][c];
                    }
                    let sv = self.s[k][n];
                    let dv = self.d[k][n];
                    out_l[k][n] = Cpx::new(
                        h[0][0] * sv.re - h[1][0] * sv.im + h[0][2] * dv.re - h[1][2] * dv.im,
                        h[0][0] * sv.im + h[1][0] * sv.re + h[0][2] * dv.im + h[1][2] * dv.re,
                    );
                    out_r[k][n] = Cpx::new(
                        h[0][1] * sv.re - h[1][1] * sv.im + h[0][3] * dv.re - h[1][3] * dv.im,
                        h[0][1] * sv.im + h[1][1] * sv.re + h[0][3] * dv.im + h[1][3] * dv.re,
                    );
                }
            }
            h_start = h_end;
        }
        self.st.h_prev = h_start;
    }
}

fn hybrid_synthesis(s: &[[Cpx; NUM_SLOTS]], x: &mut [[Cpx; 64]], cfg: usize) {
    for n in 0..NUM_SLOTS {
        let row = &mut x[n];
        if cfg == 0 {
            let mut b0 = Cpx::default();
            for sk in &s[..6] {
                b0.re += sk[n].re;
                b0.im += sk[n].im;
            }
            row[0] = b0;
            row[1] = Cpx::new(s[6][n].re + s[7][n].re, s[6][n].im + s[7][n].im);
            row[2] = Cpx::new(s[8][n].re + s[9][n].re, s[8][n].im + s[9][n].im);
            for (r, sk) in row[3..].iter_mut().zip(&s[10..]) {
                *r = sk[n];
            }
        } else {
            let sum = |a: usize, b: usize| -> Cpx {
                let mut acc = Cpx::default();
                for sk in &s[a..b] {
                    acc.re += sk[n].re;
                    acc.im += sk[n].im;
                }
                acc
            };
            row[0] = sum(0, 12);
            row[1] = sum(12, 20);
            row[2] = sum(20, 24);
            row[3] = sum(24, 28);
            row[4] = sum(28, 32);
            for k in 5..64 {
                row[k] = s[k + 27][n];
            }
        }
    }
}

/// 20 -> 34 parameter band map: 34-band parameter band b takes the 20-band band
/// MAP_20_TO_34[b] (same frequency range, from the K_TO_I_20 / K_TO_I_34 band layouts).
const MAP_20_TO_34: [u8; 34] = [
    0, 0, 1, 2, 2, 3, 4, 4, 5, 5, 6, 7, 8, 8, 9, 9, 10, 11, 12, 13, 14, 14, 15, 15, 16, 16, 17, 17,
    18, 18, 18, 18, 19, 19,
];
/// 34 -> 20 parameter band map: (source bands, weights) of the weighted average.
type Avg = (&'static [u8], &'static [u8]);
const MAP_34_TO_20: [Avg; 20] = [
    (&[0, 1], &[2, 1]),
    (&[1, 2], &[1, 2]),
    (&[3, 4], &[2, 1]),
    (&[4, 5], &[1, 2]),
    (&[6, 7], &[1, 1]),
    (&[8, 9], &[1, 1]),
    (&[10], &[1]),
    (&[11], &[1]),
    (&[12, 13], &[1, 1]),
    (&[14, 15], &[1, 1]),
    (&[16], &[1]),
    (&[17], &[1]),
    (&[18], &[1]),
    (&[19], &[1]),
    (&[20, 21], &[1, 1]),
    (&[22, 23], &[1, 1]),
    (&[24, 25], &[1, 1]),
    (&[26, 27], &[1, 1]),
    (&[28, 29, 30, 31], &[1, 1, 1, 1]),
    (&[32, 33], &[1, 1]),
];

fn map_20_to_34(h: &[f32; 34]) -> [f32; 34] {
    std::array::from_fn(|b| h[MAP_20_TO_34[b] as usize])
}

fn map_34_to_20(h: &[f32; 34]) -> [f32; 34] {
    let mut o = [0f32; 34];
    for (b, &(src, w)) in MAP_34_TO_20.iter().enumerate() {
        let s: f32 = src
            .iter()
            .zip(w)
            .map(|(&i, &wi)| h[i as usize] * wi as f32)
            .sum();
        o[b] = s / w.iter().map(|&x| x as f32).sum::<f32>();
    }
    o
}

/// Map parameters between 10/20/34-band resolutions (8.6.4.6.1, Tables 8.47..8.49).
fn map_par(p: &[i8; 34], n: usize, to: usize) -> [i8; 34] {
    let mut o = [0i8; 34];
    if to == 20 {
        match n {
            10 => {
                for b in 0..10 {
                    o[2 * b] = p[b];
                    o[2 * b + 1] = p[b];
                }
            }
            34 => {
                // 34 -> 20 (weighted average)
                for (b, &(src, w)) in MAP_34_TO_20.iter().enumerate() {
                    let s: i32 = src
                        .iter()
                        .zip(w)
                        .map(|(&i, &wi)| p[i as usize] as i32 * wi as i32)
                        .sum();
                    let d: i32 = w.iter().map(|&x| x as i32).sum();
                    o[b] = (s / d) as i8;
                }
            }
            _ => o[..20].copy_from_slice(&p[..20]),
        }
    } else {
        match n {
            10 => {
                // 10 -> 20 (band b covers 20-bands 2b, 2b + 1) composed with 20 -> 34
                for b in 0..34 {
                    o[b] = p[MAP_20_TO_34[b] as usize / 2];
                }
            }
            20 => {
                for b in 0..34 {
                    o[b] = p[MAP_20_TO_34[b] as usize];
                }
            }
            _ => o = *p,
        }
    }
    o
}

/// Map IPD / OPD parameters between 5/11/17-band resolutions (the first 5 / 11 / 17
/// parameter bands of the 10 / 20 / 34-band layouts). No selected vector carries IPD / OPD
/// in the 34-band configuration or at 5-band resolution: those maps are derived from the
/// band layouts (5 -> 11 and 5 / 11 -> 17 as for IID / ICC; bands without a source get 0)
/// and the 17 -> 11 representative-band choice is recalled; all three are unverified.
fn map_pd(p: &[i8; 17], n: usize, to: usize) -> [i8; 17] {
    let mut o = [0i8; 17];
    match (n, to) {
        (5, 11) => {
            for b in 0..5 {
                o[2 * b] = p[b];
                o[2 * b + 1] = p[b];
            }
        }
        (17, 11) => {
            const M: [usize; 11] = [0, 1, 3, 4, 6, 8, 10, 11, 12, 13, 14];
            for b in 0..11 {
                o[b] = p[M[b]];
            }
        }
        (5, 17) | (11, 17) => {
            for b in 0..17 {
                let src = if n == 5 {
                    MAP_20_TO_34[b] as usize / 2
                } else {
                    MAP_20_TO_34[b] as usize
                };
                o[b] = if src < n { p[src] } else { 0 };
            }
        }
        _ => o = *p,
    }
    o
}

fn read_par(
    br: &mut CachedBitReader<'_>,
    v: &Vlc,
    dt: bool,
    off: i32,
    n: usize,
    par: &mut [[i8; 34]; 6],
    e: usize,
) -> Result<()> {
    let (prev, cur) = par.split_at_mut(e);
    let mut acc = 0i32;
    for (p, &q) in cur[0][..n].iter_mut().zip(&prev[e - 1]) {
        let d = v
            .decode(br)
            .ok_or(DecodeError::InvalidData("PS Huffman code"))? as i32
            - off;
        // time-differential against the previous envelope, else frequency-differential
        acc = if dt { q as i32 + d } else { acc + d };
        *p = acc as i8;
    }
    Ok(())
}

fn read_ipd(
    br: &mut CachedBitReader<'_>,
    v: &Vlc,
    dt: bool,
    n: usize,
    par: &mut [[i8; 17]; 6],
    e: usize,
) -> Result<()> {
    let (prev, cur) = par.split_at_mut(e);
    let mut acc = 0i32;
    for (p, &q) in cur[0][..n].iter_mut().zip(&prev[e - 1]) {
        let d = v
            .decode(br)
            .ok_or(DecodeError::InvalidData("PS Huffman code"))? as i32;
        acc = (if dt { q as i32 + d } else { acc + d }) & 7;
        *p = acc as i8;
    }
    Ok(())
}
