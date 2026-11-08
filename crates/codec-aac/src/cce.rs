//! coupling_channel_element() (ISO/IEC 14496-3 4.4.2.1 Table 4.8, decoding 4.6.8.3).
//!
//! Dependently switched CCEs are added to their targets' spectra before or after the
//! target's TNS (cc_domain 0 / 1); independently switched CCEs are added in the time domain
//! after the target's filterbank. The coupling channel's own spectrum is TNS-filtered first.

use crate::decoder::{Core, ID_CPE};
use crate::error::{DecodeError, Result};
use crate::huffman::books;
use crate::ics::{ChannelData, CoreCfg, MAX_SFB, ZERO_HCB, parse_ics};
use triergon_bitstream::CachedBitReader;
use triergon_codec::{Budget, BudgetVec, Charged};

/// cc_scale for gain_element_scale 0..3: 2^(1/8), 2^(1/4), 2^(1/2), 2.
const CC_SCALE: [f64; 4] = [
    1.090_507_732_665_257_7,
    1.189_207_115_002_721,
    std::f64::consts::SQRT_2,
    2.0,
];

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Target {
    pub is_cpe: bool,
    pub tag: u8,
    pub l: bool,
    pub r: bool,
}

#[derive(Debug)]
pub(crate) struct CceState {
    pub tag: u8,
    pub ind_sw: bool,
    pub domain: bool,
    pub targets: BudgetVec<Target>,
    /// gains[list][g * MAX_SFB + sfb]; for independent coupling only [list][0] is used.
    pub gains: BudgetVec<[f32; 8 * MAX_SFB]>,
    pub cd: Charged<Box<ChannelData>>,
    pub overlap: [f32; 1024],
    pub prev_shape: u8,
    pub time: [f32; 1024],
    pub decoded: bool,
}

impl CceState {
    pub(crate) fn new(budget: &Budget, tag: u8) -> Result<CceState> {
        Ok(CceState {
            tag,
            ind_sw: false,
            domain: false,
            targets: BudgetVec::new(budget),
            gains: BudgetVec::try_filled(budget, [0.0; 8 * MAX_SFB], 16)?,
            cd: Charged::boxed(budget, ChannelData::new())?,
            overlap: [0.0; 1024],
            prev_shape: 0,
            time: [0.0; 1024],
            decoded: false,
        })
    }

    pub(crate) fn reset(&mut self, _aot: u8) {
        self.overlap.fill(0.0);
        self.prev_shape = 0;
    }
}

pub(crate) fn parse_cce(
    core: &mut Core,
    br: &mut CachedBitReader<'_>,
    cfg: &CoreCfg,
) -> Result<usize> {
    let tag = br.read(4) as u8;
    let ci = match core.cce.iter().position(|c| c.tag == tag) {
        Some(i) => i,
        None => {
            if core.cce.len() >= 16 {
                return Err(DecodeError::ResourceLimit("coupling channel elements"));
            }
            let c = CceState::new(&core.budget, tag)?;
            core.cce.push(Charged::boxed(&core.budget, c)?);
            core.cce.len() - 1
        }
    };
    let c: &mut CceState = &mut core.cce[ci];
    c.ind_sw = br.bit();
    let n = br.read(3) as usize + 1;
    c.targets.clear();
    c.targets.try_resize(n, Target::default())?;
    let mut lists = 0;
    for target in c.targets.iter_mut() {
        lists += 1;
        let is_cpe = br.bit();
        let tag = br.read(4) as u8;
        let (mut l, mut r) = (false, false);
        if is_cpe {
            l = br.bit();
            r = br.bit();
            if l && r {
                lists += 1;
            }
        }
        *target = Target { is_cpe, tag, l, r };
    }
    c.domain = br.bit();
    let sign = br.bit();
    let scale = CC_SCALE[br.read(2) as usize];
    parse_ics(br, cfg, false, false, &mut c.cd, &mut core.random)?;
    let vlc = &books().scalefactor;
    let info = c.cd.info;
    for list in 0..lists {
        let g0 = &mut c.gains[list];
        if list == 0 {
            for v in g0.iter_mut() {
                *v = 1.0;
            }
            continue;
        }
        let cge = if c.ind_sw { true } else { br.bit() };
        if cge {
            let a = vlc
                .decode(br)
                .ok_or(DecodeError::InvalidData("cce gain code"))? as i32
                - 60;
            let gain = scale.powi(-a) as f32;
            for v in g0.iter_mut() {
                *v = gain;
            }
        } else {
            let mut acc = 0i32;
            let mut cur = 1.0f32;
            for g in 0..info.num_window_groups {
                for sfb in 0..info.max_sfb {
                    let i = g * MAX_SFB + sfb;
                    if c.cd.band_type[i] == ZERO_HCB {
                        continue;
                    }
                    let t = vlc
                        .decode(br)
                        .ok_or(DecodeError::InvalidData("cce gain code"))?
                        as i32
                        - 60;
                    if t != 0 {
                        acc += t;
                        let (mut a, mut s) = (acc, 1.0f32);
                        if sign {
                            if a & 1 != 0 {
                                s = -1.0;
                            }
                            a >>= 1;
                        }
                        cur = s * scale.powi(-a) as f32;
                    }
                    g0[i] = cur;
                }
            }
        }
    }
    c.decoded = true;
    Ok(ci)
}

/// For each target channel (element index ei, channel ch) find the gain list index.
fn list_for(c: &CceState, kind: u8, tag: u8, ch: usize) -> Option<usize> {
    let mut idx = 0;
    for t in c.targets.iter() {
        let tkind = if t.is_cpe {
            ID_CPE
        } else {
            crate::decoder::ID_SCE
        };
        if tkind == kind && t.tag == tag {
            if !t.is_cpe {
                return if ch == 0 { Some(idx) } else { None };
            }
            return match (t.l, t.r) {
                (true, true) => Some(idx + ch),
                (true, false) => (ch == 0).then_some(idx),
                (false, true) => (ch == 1).then_some(idx),
                (false, false) => Some(idx),
            };
        }
        idx += if t.is_cpe && t.l && t.r { 2 } else { 1 };
    }
    None
}

/// Spectral-domain (dependently switched) coupling into a target channel.
pub(crate) fn apply_spectral(
    cce: &[Charged<Box<CceState>>],
    kind: u8,
    tag: u8,
    ch: usize,
    cd: &mut ChannelData,
    after_tns: bool,
) {
    for c in cce {
        if !c.decoded || c.ind_sw || c.domain != after_tns {
            continue;
        }
        let Some(list) = list_for(c, kind, tag, ch) else {
            continue;
        };
        let gains = &c.gains[list];
        let info = c.cd.info;
        let mut win = 0;
        for g in 0..info.num_window_groups {
            let glen = info.window_group_len[g] as usize;
            for sfb in 0..info.max_sfb {
                let i = g * MAX_SFB + sfb;
                if c.cd.band_type[i] == ZERO_HCB {
                    continue;
                }
                let gain = gains[i];
                let s = info.swb[sfb] as usize;
                let e = info.swb[sfb + 1] as usize;
                for w in win..win + glen {
                    for k in w * 128 + s..w * 128 + e {
                        cd.coef[k] += gain * c.cd.coef[k];
                    }
                }
            }
            win += glen;
        }
    }
}

/// Time-domain (independently switched) coupling into a target channel's output.
pub(crate) fn apply_time(
    cce: &[Charged<Box<CceState>>],
    kind: u8,
    tag: u8,
    ch: usize,
    out: &mut [f32],
) {
    for c in cce {
        if !c.decoded || !c.ind_sw {
            continue;
        }
        let Some(list) = list_for(c, kind, tag, ch) else {
            continue;
        };
        let gain = c.gains[list][0];
        for (o, s) in out.iter_mut().zip(c.time.iter()) {
            *o += gain * *s;
        }
    }
}

/// Process the coupling channels themselves (TNS, and the filterbank for independent ones).
pub(crate) fn process_cces(core: &mut Core) {
    let cfg = CoreCfg {
        aot: core.aot,
        sf_index: core.sf_index,
    };
    for c in &mut core.cce {
        let c: &mut CceState = c;
        if !c.decoded {
            continue;
        }
        crate::ics::apply_tns(&mut c.cd, &cfg);
        if c.ind_sw {
            let info = c.cd.info;
            core.fb.synth(
                &c.cd.coef,
                info.window_sequence,
                info.window_shape,
                c.prev_shape,
                &mut c.overlap,
                &mut c.time,
            );
            c.prev_shape = info.window_shape;
        }
    }
}
