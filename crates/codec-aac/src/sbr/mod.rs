//! Spectral Band Replication (ISO/IEC 14496-3 subpart 4, 4.4.2.8 syntax / 4.6.18 decoding)
//! for one SCE or CPE, including the Parametric Stereo extension (ps.rs).

mod freq;
pub(crate) mod ps;
mod qmf;
pub(crate) mod tables;

use crate::error::{DecodeError, Result};
use crate::fft::Cpx;
use crate::huffman::{Vlc, books};
use freq::FreqTables;
use tables::NOISE;
use triergon_bitstream::CachedBitReader;

const FIXFIX: u8 = 0;
const FIXVAR: u8 = 1;
const VARFIX: u8 = 2;

const NUM_TIME_SLOTS: usize = 16;
/// t_HFAdj (4.6.18.2): offset of the HF signals in QMF subsamples.
const T_HFADJ: usize = 2;
/// t_HFGen: history length of the low band buffer in QMF subsamples.
const T_HFGEN: usize = 8;
/// Length of the low band buffer: history + 32 current subsamples.
const XL_LEN: usize = T_HFGEN + 2 * NUM_TIME_SLOTS;

/// limGain (4.6.18.7.5).
const LIM_GAIN: [f32; 4] = [0.70795, 1.0, 1.41254, 10_000_000_000.0];
/// Smoothing window h_smooth (4.6.18.7.6).
#[allow(clippy::excessive_precision)] // printed precision of the source
const H_SMOOTH: [f32; 5] = [
    0.33333333333333,
    0.30150283239582,
    0.21816949906249,
    0.11516383427084,
    0.03183050093751,
];
const EPS0: f32 = 1e-12;
/// Maximum boost factor G_boost,max = 10^(2/10) (1.584893192, 4.6.18.7.5).
#[allow(clippy::excessive_precision)] // printed precision of the source
const BOOST_MAX: f32 = 1.584893192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub amp_res: bool,
    pub start_freq: u8,
    pub stop_freq: u8,
    pub xover_band: u8,
    pub freq_scale: u8,
    pub alter_scale: bool,
    pub noise_bands: u8,
    pub limiter_bands: u8,
    pub limiter_gains: u8,
    pub interpol_freq: bool,
    pub smoothing_mode: bool,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            amp_res: true,
            start_freq: 0,
            stop_freq: 0,
            xover_band: 0,
            freq_scale: 2,
            alter_scale: true,
            noise_bands: 2,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: true,
            smoothing_mode: true,
        }
    }
}

/// Per-channel SBR data and state.
#[derive(Debug)]
pub(crate) struct Chan {
    // grid of the current frame
    frame_class: u8,
    num_env: usize,
    t_env: [usize; 6],
    freq_res: [bool; 6],
    num_noise: usize,
    t_q: [usize; 3],
    /// transient envelope index l_A (-1 = none)
    l_a: i32,
    amp_res: bool,
    df_env: [bool; 5],
    df_noise: [bool; 2],
    invf: [u8; 5],
    invf_prev: [u8; 5],
    bw_prev: [f32; 5],
    /// quantised envelopes; [0] = last envelope of the previous frame
    env_q: [[i32; 49]; 6],
    /// frequency resolution of env_q[0]
    env_prev_res: bool,
    noise_q: [[i32; 5]; 3],
    add_harmonic_flag: bool,
    add_harmonic: [bool; 49],
    /// sinusoid mapping of the last envelope of the previous frame (per subband - kx)
    s_index_prev: [bool; 64],
    // dequantised
    e_orig: [[f32; 49]; 5],
    q_orig: [[f32; 5]; 2],
    // previous frame state
    prev_num_env: usize,
    prev_t_env_last: usize,
    /// l_A of the previous frame expressed relative to the current frame (0 or -1)
    l_a_prev: i32,
    // signal state
    analysis: qmf::Analysis,
    synthesis: qmf::Synthesis,
    /// low band history: xl[k][t], t < T_HFGEN valid
    xl: Vec<[Cpx; XL_LEN]>,
    /// Y of the previous frame, slots 32..38 (high band contribution reaching into this frame)
    y_prev: Vec<[Cpx; 64]>,
    /// gain / noise smoothing history (4 slots) per subband
    g_hist: [[f32; 64]; 4],
    q_hist: [[f32; 64]; 4],
    index_noise: usize,
    index_sine: usize,
    /// kx / M used for the previous frame's high band
    kx_prev: usize,
    m_prev: usize,
    xa: Vec<[Cpx; 32]>,
    xh: Vec<[Cpx; 40]>,
    y: Vec<[Cpx; 64]>,
}

impl Chan {
    fn new() -> Chan {
        Chan {
            frame_class: 0,
            num_env: 0,
            t_env: [0; 6],
            freq_res: [false; 6],
            num_noise: 0,
            t_q: [0; 3],
            l_a: -1,
            amp_res: false,
            df_env: [false; 5],
            df_noise: [false; 2],
            invf: [0; 5],
            invf_prev: [0; 5],
            bw_prev: [0.0; 5],
            env_q: [[0; 49]; 6],
            env_prev_res: false,
            noise_q: [[0; 5]; 3],
            add_harmonic_flag: false,
            add_harmonic: [false; 49],
            s_index_prev: [false; 64],
            e_orig: [[0.0; 49]; 5],
            q_orig: [[0.0; 5]; 2],
            prev_num_env: 0,
            prev_t_env_last: NUM_TIME_SLOTS,
            l_a_prev: -1,
            analysis: qmf::Analysis::new(),
            synthesis: qmf::Synthesis::new(),
            xl: vec![[Cpx::default(); XL_LEN]; 32],
            y_prev: vec![[Cpx::default(); 64]; 8],
            g_hist: [[0.0; 64]; 4],
            q_hist: [[0.0; 64]; 4],
            index_noise: 0,
            index_sine: 0,
            kx_prev: 32,
            m_prev: 0,
            xa: vec![[Cpx::default(); 32]; 32],
            xh: vec![[Cpx::default(); 40]; 64],
            y: vec![[Cpx::default(); 64]; 40],
        }
    }

    fn reset_signal(&mut self) {
        self.analysis.reset();
        self.synthesis.reset();
        for r in &mut self.xl {
            *r = [Cpx::default(); XL_LEN];
        }
        for r in &mut self.y_prev {
            *r = [Cpx::default(); 64];
        }
        self.prev_t_env_last = NUM_TIME_SLOTS;
        self.prev_num_env = 0;
        self.l_a_prev = -1;
        self.kx_prev = 32;
        self.m_prev = 0;
        self.env_q = [[0; 49]; 6];
        self.noise_q = [[0; 5]; 3];
        self.bw_prev = [0.0; 5];
        self.invf_prev = [0; 5];
        self.s_index_prev = [false; 64];
        self.index_noise = 0;
        self.index_sine = 0;
    }
}

#[derive(Debug)]
pub(crate) struct Sbr {
    fs: u32,
    hdr: Header,
    have_header: bool,
    /// frequency tables changed in this frame (4.6.18.3.1 reset)
    reset: bool,
    ft: FreqTables,
    coupling: bool,
    ch: [Box<Chan>; 2],
    /// data of the current frame was parsed successfully
    data_ok: bool,
    pub(crate) ps: Box<ps::Ps>,
    ps_seen: bool,
    // scratch
    x: Vec<[Cpx; 64]>,
    x_r: Vec<[Cpx; 64]>,
}

impl Sbr {
    pub(crate) fn new(core_rate: u32) -> Sbr {
        Sbr {
            fs: core_rate * 2,
            hdr: Header::default(),
            have_header: false,
            reset: false,
            ft: FreqTables::default(),
            coupling: false,
            ch: [Box::new(Chan::new()), Box::new(Chan::new())],
            data_ok: false,
            ps: Box::new(ps::Ps::new()),
            ps_seen: false,
            x: vec![[Cpx::default(); 64]; 38],
            x_r: vec![[Cpx::default(); 64]; 38],
        }
    }

    pub(crate) fn reset(&mut self) {
        for c in &mut self.ch {
            c.reset_signal();
        }
        self.ps.reset();
    }

    /// sbr_extension_data(): returns true when a PS extension was found.
    pub(crate) fn parse(
        &mut self,
        br: &mut CachedBitReader<'_>,
        crc: bool,
        bits: usize,
        is_cpe: bool,
    ) -> Result<bool> {
        let start = br.position();
        self.data_ok = false;
        if crc {
            br.skip(10); // bs_sbr_crc_bits (not verified, like the reference decoder)
        }
        if br.bit() {
            self.parse_header(br)?;
        }
        if !self.have_header {
            // data without a preceding header cannot be decoded: skip it
            let used = br.position() - start;
            if used < bits {
                br.skip(bits - used);
            }
            return Ok(false);
        }
        let r = if is_cpe {
            self.parse_cpe(br, start + bits)
        } else {
            self.parse_sce(br, start + bits)
        };
        match r {
            Ok(ps) => {
                if br.position() > start + bits {
                    return Err(DecodeError::InvalidData("SBR data overrun"));
                }
                self.data_ok = true;
                Ok(ps)
            }
            Err(e) => Err(e),
        }
    }

    fn parse_header(&mut self, br: &mut CachedBitReader<'_>) -> Result<()> {
        let mut h = Header {
            amp_res: br.bit(),
            start_freq: br.read(4) as u8,
            stop_freq: br.read(4) as u8,
            xover_band: br.read(3) as u8,
            ..Header::default()
        };
        br.skip(2); // bs_reserved
        let extra1 = br.bit();
        let extra2 = br.bit();
        if extra1 {
            h.freq_scale = br.read(2) as u8;
            h.alter_scale = br.bit();
            h.noise_bands = br.read(2) as u8;
        }
        if extra2 {
            h.limiter_bands = br.read(2) as u8;
            h.limiter_gains = br.read(2) as u8;
            h.interpol_freq = br.bit();
            h.smoothing_mode = br.bit();
        }
        let old = self.hdr;
        let need_reset = !self.have_header
            || h.start_freq != old.start_freq
            || h.stop_freq != old.stop_freq
            || h.xover_band != old.xover_band
            || h.freq_scale != old.freq_scale
            || h.alter_scale != old.alter_scale
            || h.noise_bands != old.noise_bands;
        self.hdr = h;
        if need_reset {
            match freq::compute(&h, self.fs) {
                Ok(ft) => {
                    self.ft = ft;
                    self.have_header = true;
                    self.reset = true;
                }
                Err(e) => {
                    self.have_header = false;
                    return Err(e);
                }
            }
        } else if h.limiter_bands != old.limiter_bands {
            self.ft.lim = freq::limiter_table(
                h.limiter_bands,
                &self.ft.low,
                self.ft.kx,
                &self.ft.patch_num,
            );
            self.ft.n_lim = self.ft.lim.len() - 1;
        }
        Ok(())
    }

    fn parse_sce(&mut self, br: &mut CachedBitReader<'_>, end: usize) -> Result<bool> {
        if br.bit() {
            br.skip(4);
        }
        self.coupling = false;
        let ft = &self.ft;
        let c = &mut self.ch[0];
        parse_grid(br, c)?;
        parse_dtdf(br, c);
        parse_invf(br, c, ft.n_q);
        parse_envelope(br, c, ft, self.hdr.amp_res, false)?;
        parse_noise(br, c, ft, false)?;
        c.add_harmonic_flag = br.bit();
        if c.add_harmonic_flag {
            c.add_harmonic[..ft.n_high]
                .iter_mut()
                .for_each(|h| *h = br.bit());
        } else {
            c.add_harmonic = [false; 49];
        }
        self.parse_extended(br, end)
    }

    fn parse_cpe(&mut self, br: &mut CachedBitReader<'_>, end: usize) -> Result<bool> {
        if br.bit() {
            br.skip(8);
        }
        self.coupling = br.bit();
        let ft = &self.ft;
        let amp = self.hdr.amp_res;
        let (a, b) = self.ch.split_at_mut(1);
        let (c0, c1) = (&mut a[0], &mut b[0]);
        if self.coupling {
            parse_grid(br, c0)?;
            copy_grid(c0, c1);
            parse_dtdf(br, c0);
            parse_dtdf(br, c1);
            parse_invf(br, c0, ft.n_q);
            c1.invf = c0.invf;
            parse_envelope(br, c0, ft, amp, false)?;
            parse_noise(br, c0, ft, false)?;
            parse_envelope(br, c1, ft, amp, true)?;
            parse_noise(br, c1, ft, true)?;
        } else {
            parse_grid(br, c0)?;
            parse_grid(br, c1)?;
            parse_dtdf(br, c0);
            parse_dtdf(br, c1);
            parse_invf(br, c0, ft.n_q);
            parse_invf(br, c1, ft.n_q);
            parse_envelope(br, c0, ft, amp, false)?;
            parse_envelope(br, c1, ft, amp, false)?;
            parse_noise(br, c0, ft, false)?;
            parse_noise(br, c1, ft, false)?;
        }
        for c in [c0, c1] {
            c.add_harmonic_flag = br.bit();
            if c.add_harmonic_flag {
                c.add_harmonic[..ft.n_high]
                    .iter_mut()
                    .for_each(|h| *h = br.bit());
            } else {
                c.add_harmonic = [false; 49];
            }
        }
        self.parse_extended(br, end)
    }

    fn parse_extended(&mut self, br: &mut CachedBitReader<'_>, end: usize) -> Result<bool> {
        let mut ps = false;
        let ext = br.bit();
        if ext {
            let mut cnt = br.read(4) as usize;
            if cnt == 15 {
                cnt += br.read(8) as usize;
            }
            let mut left = 8 * cnt as isize;
            if br.position() as isize + left > end as isize {
                return Err(DecodeError::InvalidData("SBR extended data length"));
            }
            while left > 7 {
                let id = br.read(2);
                left -= 2;
                if id == 2 {
                    // EXTENSION_ID_PS
                    let before = br.position();
                    let used = self.ps.parse(br, left as usize)?;
                    let _ = used;
                    left -= (br.position() - before) as isize;
                    ps = true;
                    self.ps_seen = true;
                } else {
                    br.skip(left.max(0) as usize);
                    left = 0;
                }
            }
            if left > 0 {
                br.skip(left as usize);
            }
        }
        Ok(ps)
    }

    /// Apply SBR (and PS) to one element. `l`/`r` hold 1024 core samples on input and
    /// receive 2048 output samples.
    pub(crate) fn process(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        nch: usize,
        ps: bool,
        _cpe: bool,
    ) {
        let ready = self.have_header && self.data_ok;
        let reset = self.reset && ready;
        if ready {
            for c in 0..nch {
                dequant(&mut self.ch[..], c, &self.ft, self.coupling, nch);
            }
        }
        for c in 0..nch {
            let buf: &mut [f32] = if c == 0 { &mut l[..] } else { &mut r[..] };
            let ch = &mut self.ch[c];
            // QMF analysis of the core output (float scale -> 16-bit domain)
            for t in 0..32 {
                let mut tmp = [Cpx::default(); 32];
                ch.analysis
                    .run(&buf[t * 32..t * 32 + 32], 32768.0, &mut tmp);
                ch.xa[t] = tmp;
            }
            let (kx, m) = if ready {
                (self.ft.kx, self.ft.m)
            } else {
                (32, 0)
            };
            // low band buffer: history (previous frame, k < kx_prev) + current (k < kx)
            for k in 0..32 {
                for t in 0..T_HFGEN {
                    if k >= ch.kx_prev {
                        ch.xl[k][t] = Cpx::default();
                    }
                }
                for t in 0..32 {
                    ch.xl[k][T_HFGEN + t] = if k < kx { ch.xa[t][k] } else { Cpx::default() };
                }
            }
            if ready {
                hf_generate(ch, &self.ft);
                hf_adjust(ch, &self.ft, &self.hdr, reset);
            }
            // assemble the synthesis input X(k, i), i < 32
            let i_temp = (2 * ch.prev_t_env_last).saturating_sub(32).min(8);
            for i in 0..32 {
                let row = &mut self.x[i];
                *row = [Cpx::default(); 64];
                let (kxi, mi) = if i < i_temp {
                    (ch.kx_prev, ch.m_prev)
                } else {
                    (kx, m)
                };
                for (r, xl) in row.iter_mut().zip(&ch.xl[..kxi.min(32)]) {
                    *r = xl[i + T_HFADJ];
                }
                let hf = kxi..(kxi + mi).min(64);
                if i < i_temp {
                    row[hf.clone()].copy_from_slice(&ch.y_prev[i][hf]);
                } else if ready {
                    row[hf.clone()].copy_from_slice(&ch.y[i][hf]);
                }
            }
            // keep state for the next frame
            for xl in &mut ch.xl {
                xl.copy_within(32..32 + T_HFGEN, 0);
            }
            if ready {
                let last = ch.t_env[ch.num_env];
                for i in 0..8 {
                    ch.y_prev[i] = if 32 + i < 2 * last {
                        ch.y[32 + i]
                    } else {
                        [Cpx::default(); 64]
                    };
                }
                ch.prev_t_env_last = last;
                ch.kx_prev = kx;
                ch.m_prev = m;
            } else {
                ch.prev_t_env_last = NUM_TIME_SLOTS;
                ch.kx_prev = kx;
                ch.m_prev = 0;
            }
            if c == 0 && ps {
                // look-ahead slots (low band only) for the PS hybrid filters
                for i in 32..38 {
                    let row = &mut self.x[i];
                    *row = [Cpx::default(); 64];
                    for k in 0..kx.min(32) {
                        row[k] = ch.xl[k][i + T_HFADJ];
                    }
                }
                // Parametric stereo: mono QMF matrix -> left/right matrices (8.6.4)
                if self.ps_seen {
                    self.ps.apply(&mut self.x, &mut self.x_r, ready);
                } else {
                    // no PS data yet: both outputs carry the mono signal
                    for i in 0..32 {
                        self.x_r[i] = self.x[i];
                    }
                }
                for i in 0..32 {
                    let x = self.x[i];
                    self.ch[0]
                        .synthesis
                        .run(&x, 1.0 / 32768.0, &mut l[i * 64..i * 64 + 64]);
                }
                for i in 0..32 {
                    let x = self.x_r[i];
                    self.ch[1]
                        .synthesis
                        .run(&x, 1.0 / 32768.0, &mut r[i * 64..i * 64 + 64]);
                }
                continue;
            }
            let chn = &mut self.ch[c];
            let out: &mut [f32] = if c == 0 { &mut l[..] } else { &mut r[..] };
            for i in 0..32 {
                let x = self.x[i];
                chn.synthesis
                    .run(&x, 1.0 / 32768.0, &mut out[i * 64..i * 64 + 64]);
            }
        }
        if ready {
            self.reset = false;
        }
        self.data_ok = false;
    }
}

fn copy_grid(a: &Chan, b: &mut Chan) {
    // like parse_grid: the transient position of the previous frame, before num_env changes
    b.l_a_prev = if b.l_a >= 0 && b.l_a as usize == b.num_env {
        0
    } else {
        -1
    };
    b.frame_class = a.frame_class;
    b.num_env = a.num_env;
    b.t_env = a.t_env;
    b.freq_res = a.freq_res;
    b.num_noise = a.num_noise;
    b.t_q = a.t_q;
    b.l_a = a.l_a;
    b.amp_res = a.amp_res;
}

/// sbr_grid() and the derived time borders (4.6.18.3.3).
fn parse_grid(br: &mut CachedBitReader<'_>, c: &mut Chan) -> Result<()> {
    // a transient at the end of the previous frame affects this frame's first envelope
    c.l_a_prev = if c.l_a >= 0 && c.l_a as usize == c.num_env {
        0
    } else {
        -1
    };
    let class = br.read(2) as u8;
    let mut rel0 = [0usize; 4];
    let mut rel1 = [0usize; 4];
    let (mut nrel0, mut nrel1) = (0usize, 0usize);
    let (abs0, abs1);
    let mut pointer = 0usize;
    let num_env;
    match class {
        FIXFIX => {
            let tmp = br.read(2);
            num_env = 1usize << tmp;
            if num_env > 4 {
                return Err(DecodeError::InvalidData("SBR FIXFIX envelopes"));
            }
            let fr = br.bit();
            c.freq_res[..num_env].fill(fr);
            abs0 = 0;
            abs1 = NUM_TIME_SLOTS;
            nrel0 = num_env - 1;
            let rel = NUM_TIME_SLOTS / num_env;
            for r in rel0.iter_mut().take(nrel0) {
                *r = rel;
            }
        }
        FIXVAR => {
            abs1 = br.read(2) as usize + NUM_TIME_SLOTS;
            nrel1 = br.read(2) as usize;
            num_env = nrel1 + 1;
            for r in rel1.iter_mut().take(nrel1) {
                *r = 2 * br.read(2) as usize + 2;
            }
            pointer = br.read(ptr_bits(num_env)) as usize;
            for e in 0..num_env {
                c.freq_res[num_env - 1 - e] = br.bit();
            }
            abs0 = 0;
        }
        VARFIX => {
            abs0 = br.read(2) as usize;
            nrel0 = br.read(2) as usize;
            num_env = nrel0 + 1;
            for r in rel0.iter_mut().take(nrel0) {
                *r = 2 * br.read(2) as usize + 2;
            }
            pointer = br.read(ptr_bits(num_env)) as usize;
            c.freq_res[..num_env].iter_mut().for_each(|f| *f = br.bit());
            abs1 = NUM_TIME_SLOTS;
        }
        _ => {
            // VARVAR
            abs0 = br.read(2) as usize;
            abs1 = br.read(2) as usize + NUM_TIME_SLOTS;
            nrel0 = br.read(2) as usize;
            nrel1 = br.read(2) as usize;
            num_env = nrel0 + nrel1 + 1;
            if num_env > 5 {
                return Err(DecodeError::InvalidData("SBR VARVAR envelopes"));
            }
            for r in rel0.iter_mut().take(nrel0) {
                *r = 2 * br.read(2) as usize + 2;
            }
            for r in rel1.iter_mut().take(nrel1) {
                *r = 2 * br.read(2) as usize + 2;
            }
            pointer = br.read(ptr_bits(num_env)) as usize;
            c.freq_res[..num_env].iter_mut().for_each(|f| *f = br.bit());
        }
    }
    if pointer > num_env {
        return Err(DecodeError::InvalidData("SBR bs_pointer"));
    }
    c.frame_class = class;
    c.num_env = num_env;
    c.t_env[0] = abs0;
    c.t_env[num_env] = abs1;
    for l in 1..=nrel0 {
        c.t_env[l] = c.t_env[l - 1] + rel0[l - 1];
    }
    let mut l = num_env;
    for &r in &rel1[..nrel1] {
        l -= 1;
        c.t_env[l] = c.t_env[l + 1] - r.min(c.t_env[l + 1]);
    }
    for l in 0..num_env {
        if c.t_env[l] >= c.t_env[l + 1] {
            return Err(DecodeError::InvalidData("SBR time borders"));
        }
    }
    if c.t_env[num_env] > NUM_TIME_SLOTS + 3 {
        return Err(DecodeError::InvalidData("SBR time borders"));
    }
    c.num_noise = if num_env > 1 { 2 } else { 1 };
    c.t_q[0] = c.t_env[0];
    if num_env == 1 {
        c.t_q[1] = c.t_env[1];
    } else {
        let mid = match class {
            FIXFIX => num_env / 2,
            VARFIX => {
                if pointer == 0 {
                    1
                } else if pointer == 1 {
                    num_env - 1
                } else {
                    pointer - 1
                }
            }
            _ => {
                if pointer > 1 {
                    num_env + 1 - pointer
                } else {
                    num_env - 1
                }
            }
        };
        c.t_q[1] = c.t_env[mid];
        c.t_q[2] = c.t_env[num_env];
    }
    c.l_a = match class {
        FIXFIX => -1,
        VARFIX => {
            if pointer > 1 {
                pointer as i32 - 1
            } else {
                -1
            }
        }
        _ => {
            if pointer > 0 {
                num_env as i32 + 1 - pointer as i32
            } else {
                -1
            }
        }
    };
    // FIXFIX with a single envelope uses 1.5 dB resolution regardless of bs_amp_res
    c.amp_res = !(class == FIXFIX && num_env == 1);
    Ok(())
}

fn ptr_bits(num_env: usize) -> u32 {
    // ceil(log2(num_env + 1))
    let mut b = 0;
    while (1usize << b) < num_env + 1 {
        b += 1;
    }
    b
}

fn parse_dtdf(br: &mut CachedBitReader<'_>, c: &mut Chan) {
    for e in 0..c.num_env {
        c.df_env[e] = br.bit();
    }
    for n in 0..c.num_noise {
        c.df_noise[n] = br.bit();
    }
}

fn parse_invf(br: &mut CachedBitReader<'_>, c: &mut Chan, n_q: usize) {
    c.invf[..n_q].iter_mut().for_each(|v| *v = br.read(2) as u8);
}

fn huff(br: &mut CachedBitReader<'_>, v: &Vlc) -> Result<i32> {
    v.decode(br)
        .map(|s| s as i32)
        .ok_or(DecodeError::InvalidData("SBR Huffman code"))
}

/// sbr_envelope() with delta decoding (4.6.18.3.4).
fn parse_envelope(
    br: &mut CachedBitReader<'_>,
    c: &mut Chan,
    ft: &FreqTables,
    hdr_amp: bool,
    balance: bool,
) -> Result<()> {
    let bk = books();
    let amp = hdr_amp && c.amp_res;
    c.amp_res = amp;
    // tables: (t, f, lav, start bits)
    let (t, f, lav, bits) = match (amp, balance) {
        (false, false) => (&bk.sbr[0], &bk.sbr[1], 60, 7),
        (false, true) => (&bk.sbr[2], &bk.sbr[3], 24, 6),
        (true, false) => (&bk.sbr[4], &bk.sbr[5], 31, 6),
        (true, true) => (&bk.sbr[6], &bk.sbr[7], 12, 5),
    };
    let delta = if balance { 2 } else { 1 };
    let n_res = |r: bool| if r { ft.n_high } else { ft.n_low };
    for e in 0..c.num_env {
        let res = c.freq_res[e];
        let n = n_res(res);
        let prev_res = if e == 0 {
            c.env_prev_res
        } else {
            c.freq_res[e - 1]
        };
        if c.df_env[e] {
            for j in 0..n {
                let d = delta * (huff(br, t)? - lav);
                let base = if res == prev_res {
                    c.env_q[e][j]
                } else if res {
                    // high from low: f_low[k] <= f_high[j] < f_low[k + 1]
                    let k = (j + (ft.n_high & 1)) >> 1;
                    c.env_q[e][k]
                } else {
                    // low from high: f_high[k] == f_low[j]
                    let k = if j == 0 { 0 } else { 2 * j - (ft.n_high & 1) };
                    c.env_q[e][k]
                };
                c.env_q[e + 1][j] = base + d;
            }
        } else {
            c.env_q[e + 1][0] = delta * br.read(bits) as i32;
            for j in 1..n {
                let d = delta * (huff(br, f)? - lav);
                c.env_q[e + 1][j] = c.env_q[e + 1][j - 1] + d;
            }
        }
    }
    Ok(())
}

/// sbr_noise() with delta decoding.
fn parse_noise(
    br: &mut CachedBitReader<'_>,
    c: &mut Chan,
    ft: &FreqTables,
    balance: bool,
) -> Result<()> {
    let bk = books();
    let (t, f, lav) = if balance {
        (&bk.sbr[9], &bk.sbr[7], 12)
    } else {
        (&bk.sbr[8], &bk.sbr[5], 31)
    };
    let delta = if balance { 2 } else { 1 };
    for n in 0..c.num_noise {
        if c.df_noise[n] {
            for j in 0..ft.n_q {
                let d = delta * (huff(br, t)? - lav);
                c.noise_q[n + 1][j] = c.noise_q[n][j] + d;
            }
        } else {
            c.noise_q[n + 1][0] = delta * br.read(5) as i32;
            for j in 1..ft.n_q {
                let d = delta * (huff(br, f)? - lav);
                c.noise_q[n + 1][j] = c.noise_q[n + 1][j - 1] + d;
            }
        }
    }
    Ok(())
}

/// Envelope / noise dequantisation (4.6.18.3.5) incl. the coupled stereo case, then the
/// bookkeeping that makes the last envelope / noise floor the "previous" for delta decoding.
fn dequant(chans: &mut [Box<Chan>], ci: usize, ft: &FreqTables, coupling: bool, nch: usize) {
    if coupling && nch == 2 {
        if ci == 1 {
            return; // both channels handled together with ci == 0
        }
        let (a, b) = chans.split_at_mut(1);
        let (c0, c1) = (&mut a[0], &mut b[0]);
        let alpha = if c0.amp_res { 1.0f32 } else { 0.5 };
        let pan = if c0.amp_res { 12.0f32 } else { 24.0 };
        for e in 0..c0.num_env {
            let n = if c0.freq_res[e] { ft.n_high } else { ft.n_low };
            for k in 0..n {
                let t1 = (c0.env_q[e + 1][k] as f32 * alpha + 7.0).exp2();
                let t2 = ((pan - c1.env_q[e + 1][k] as f32) * alpha).exp2();
                if t1 > 2.0f32.powi(46) {
                    // out of range: treat as zero like an invalid frame would be muted
                }
                c0.e_orig[e][k] = t1 / (1.0 + t2);
                c1.e_orig[e][k] = c0.e_orig[e][k] * t2;
            }
        }
        for nn in 0..c0.num_noise {
            for k in 0..ft.n_q {
                let t1 = (6.0 - c0.noise_q[nn + 1][k] as f32 + 1.0).exp2();
                let t2 = (12.0 - c1.noise_q[nn + 1][k] as f32).exp2();
                c0.q_orig[nn][k] = t1 / (1.0 + t2);
                c1.q_orig[nn][k] = c0.q_orig[nn][k] * t2;
            }
        }
        for c in [c0, c1] {
            finish_dequant(c);
        }
        return;
    }
    let c = &mut chans[ci];
    let alpha = if c.amp_res { 1.0f32 } else { 0.5 };
    for e in 0..c.num_env {
        let n = if c.freq_res[e] { ft.n_high } else { ft.n_low };
        for k in 0..n {
            c.e_orig[e][k] = (c.env_q[e + 1][k] as f32 * alpha + 6.0).exp2();
        }
    }
    for nn in 0..c.num_noise {
        for k in 0..ft.n_q {
            c.q_orig[nn][k] = (6.0 - c.noise_q[nn + 1][k] as f32).exp2();
        }
    }
    finish_dequant(c);
}

fn finish_dequant(c: &mut Chan) {
    // last envelope / noise floor become the reference for the next frame's delta coding
    let ne = c.num_env;
    c.env_q[0] = c.env_q[ne];
    c.env_prev_res = c.freq_res[ne - 1];
    c.noise_q[0] = c.noise_q[c.num_noise];
}

/// Chirp factors per noise band (4.6.18.6.2, Table 4.177/4.178).
fn chirp(c: &mut Chan, n_q: usize) -> [f32; 5] {
    let mut bw = [0f32; 5];
    for i in 0..n_q {
        let new = match (c.invf[i], c.invf_prev[i]) {
            (0, 1) => 0.6,
            (0, _) => 0.0,
            (1, 0) => 0.6,
            (1, _) => 0.75,
            (2, _) => 0.9,
            _ => 0.98,
        };
        let mut b = if new < c.bw_prev[i] {
            0.75 * new + 0.25 * c.bw_prev[i]
        } else {
            0.90625 * new + 0.09375 * c.bw_prev[i]
        };
        if b < 0.015625 {
            b = 0.0;
        }
        b = b.min(0.99609375);
        bw[i] = b;
        c.bw_prev[i] = b;
        c.invf_prev[i] = c.invf[i];
    }
    bw
}

/// HF generation (4.6.18.6): LPC estimation on the low band and patching.
fn hf_generate(c: &mut Chan, ft: &FreqTables) {
    let bw = chirp(c, ft.n_q);
    let s0 = 2 * c.t_env[0] + T_HFADJ;
    let s1 = 2 * c.t_env[c.num_env] + T_HFADJ;
    let mut k = ft.kx;
    for (p, &num) in ft.patch_num.iter().enumerate() {
        for x in 0..num {
            let src = ft.patch_start[p] + x;
            let (a0, a1) = lpc(&c.xl[src]);
            let mut g = 0;
            while g + 1 < ft.noise.len() - 1 && k >= ft.noise[g + 1] {
                g += 1;
            }
            let b = bw[g];
            let (a0, a1) = if b > 0.0 {
                (
                    Cpx::new(a0.re * b, a0.im * b),
                    Cpx::new(a1.re * b * b, a1.im * b * b),
                )
            } else {
                (Cpx::default(), Cpx::default())
            };
            let xl = &c.xl[src];
            let xh = &mut c.xh[k];
            for s in s0..s1 {
                let x0 = xl[s];
                let x1 = xl[s - 1];
                let x2 = xl[s - 2];
                xh[s] = Cpx::new(
                    x0.re + a0.re * x1.re - a0.im * x1.im + a1.re * x2.re - a1.im * x2.im,
                    x0.im + a0.re * x1.im + a0.im * x1.re + a1.re * x2.im + a1.im * x2.re,
                );
            }
            k += 1;
        }
    }
}

/// Covariance-method second order prediction coefficients (4.6.18.6.2).
fn lpc(x: &[Cpx; XL_LEN]) -> (Cpx, Cpx) {
    // phi(i, j) = sum_{n=0}^{37} x[n + 2 - i] conj(x[n + 2 - j])
    let mut r01 = Cpx::default();
    let mut r02 = Cpx::default();
    let mut r11 = 0f32;
    let mut r12 = Cpx::default();
    let mut r22 = 0f32;
    for n in 0..(2 * NUM_TIME_SLOTS + 6) {
        let a = x[n + 2];
        let b = x[n + 1];
        let cc = x[n];
        // phi(0,1) = sum a conj(b)
        r01.re += a.re * b.re + a.im * b.im;
        r01.im += a.im * b.re - a.re * b.im;
        r02.re += a.re * cc.re + a.im * cc.im;
        r02.im += a.im * cc.re - a.re * cc.im;
        r11 += b.re * b.re + b.im * b.im;
        r12.re += b.re * cc.re + b.im * cc.im;
        r12.im += b.im * cc.re - b.re * cc.im;
        r22 += cc.re * cc.re + cc.im * cc.im;
    }
    let det = r22 * r11 - (r12.re * r12.re + r12.im * r12.im) / (1.0 + 1e-6);
    let mut a1 = Cpx::default();
    if det != 0.0 {
        // alpha1 = (phi01 phi12 - phi02 phi11) / det
        a1.re = (r01.re * r12.re - r01.im * r12.im - r02.re * r11) / det;
        a1.im = (r01.im * r12.re + r01.re * r12.im - r02.im * r11) / det;
    }
    let mut a0 = Cpx::default();
    if r11 != 0.0 {
        // alpha0 = -(phi01 + alpha1 conj(phi12)) / phi11
        a0.re = -(r01.re + a1.re * r12.re + a1.im * r12.im) / r11;
        a0.im = -(r01.im + a1.im * r12.re - a1.re * r12.im) / r11;
    }
    if a0.re * a0.re + a0.im * a0.im >= 16.0 || a1.re * a1.re + a1.im * a1.im >= 16.0 {
        return (Cpx::default(), Cpx::default());
    }
    (a0, a1)
}

/// HF adjustment (4.6.18.7): mapping, gains, limiter, smoothing, noise and sinusoids.
fn hf_adjust(c: &mut Chan, ft: &FreqTables, hdr: &Header, reset: bool) {
    let kx = ft.kx;
    let m = ft.m;
    let ne = c.num_env;
    let mut e_orig_m = [[0f32; 64]; 5];
    let mut q_m = [[0f32; 64]; 5];
    let mut s_idx = [[false; 64]; 5];
    let mut s_map = [[false; 64]; 5];
    let mut e_curr = [[0f32; 64]; 5];
    // mapping (4.6.18.7.2)
    for e in 0..ne {
        let table: &[usize] = if c.freq_res[e] { &ft.high } else { &ft.low };
        let n = table.len() - 1;
        for i in 0..n {
            for mm in table[i]..table[i + 1] {
                e_orig_m[e][mm - kx] = c.e_orig[e][i];
            }
        }
        let nf = if c.num_noise > 1 && c.t_env[e] >= c.t_q[1] {
            1
        } else {
            0
        };
        for i in 0..ft.n_q {
            for mm in ft.noise[i]..ft.noise[i + 1] {
                q_m[e][mm - kx] = c.q_orig[nf][i];
            }
        }
        for i in 0..ft.n_high {
            let mid = (ft.high[i] + ft.high[i + 1]) >> 1;
            // a sinusoid starts at the transient envelope, or continues from the last
            // envelope of the previous frame (4.6.18.7.2)
            s_idx[e][mid - kx] =
                c.add_harmonic[i] && (c.l_a < 0 || e as i32 >= c.l_a || c.s_index_prev[mid - kx]);
        }
        for i in 0..n {
            let any = (table[i]..table[i + 1]).any(|mm| s_idx[e][mm - kx]);
            for mm in table[i]..table[i + 1] {
                s_map[e][mm - kx] = any;
            }
        }
    }
    // current energies (4.6.18.7.3)
    for e in 0..ne {
        let lb = 2 * c.t_env[e] + T_HFADJ;
        let ub = 2 * c.t_env[e + 1] + T_HFADJ;
        if hdr.interpol_freq {
            let inv = 1.0 / (ub - lb) as f32;
            for mm in 0..m {
                let s: f32 = c.xh[mm + kx][lb..ub]
                    .iter()
                    .map(|v| v.re * v.re + v.im * v.im)
                    .sum();
                e_curr[e][mm] = s * inv;
            }
        } else {
            let table: &[usize] = if c.freq_res[e] { &ft.high } else { &ft.low };
            for i in 0..table.len() - 1 {
                let mut s = 0f32;
                for xh in &c.xh[table[i]..table[i + 1]] {
                    for v in &xh[lb..ub] {
                        s += v.re * v.re + v.im * v.im;
                    }
                }
                s /= ((ub - lb) * (table[i + 1] - table[i])) as f32;
                for k in table[i]..table[i + 1] {
                    e_curr[e][k - kx] = s;
                }
            }
        }
    }
    // gains (4.6.18.7.4) and limiter (4.6.18.7.5)
    let mut gain = [[0f32; 64]; 5];
    let mut qm = [[0f32; 64]; 5];
    let mut sm = [[0f32; 64]; 5];
    // Limiter pruning can drop the final low-band boundary after a short
    // patch is discarded. No gain, noise, or sinusoid is defined above
    // the last retained limiter boundary.
    let adjusted_bands = ft.lim[ft.n_lim] - kx;
    for e in 0..ne {
        let delta = !(e as i32 == c.l_a || e as i32 == c.l_a_prev);
        for mm in 0..adjusted_bands {
            let eo = e_orig_m[e][mm];
            let q = q_m[e][mm];
            let ec = e_curr[e][mm];
            let t = eo / (1.0 + q);
            qm[e][mm] = (t * q).sqrt();
            sm[e][mm] = if s_idx[e][mm] { t.sqrt() } else { 0.0 };
            gain[e][mm] = if !s_map[e][mm] {
                (eo / ((1.0 + ec) * (1.0 + if delta { q } else { 0.0 }))).sqrt()
            } else {
                (eo * q / ((1.0 + ec) * (1.0 + q))).sqrt()
            };
        }
        for k in 0..ft.n_lim {
            let lo = ft.lim[k] - kx;
            let hi = ft.lim[k + 1] - kx;
            let (mut so, mut sc) = (0f32, 0f32);
            for mm in lo..hi {
                so += e_orig_m[e][mm];
                sc += e_curr[e][mm];
            }
            let gmax = (LIM_GAIN[hdr.limiter_gains as usize] * ((EPS0 + so) / (EPS0 + sc)).sqrt())
                .min(1.0e5);
            for mm in lo..hi {
                let qmax = qm[e][mm] * gmax / gain[e][mm];
                if qmax < qm[e][mm] {
                    qm[e][mm] = qmax;
                }
                gain[e][mm] = gain[e][mm].min(gmax);
            }
            let (mut so, mut sd) = (0f32, 0f32);
            for mm in lo..hi {
                so += e_orig_m[e][mm];
                sd += e_curr[e][mm] * gain[e][mm] * gain[e][mm]
                    + sm[e][mm] * sm[e][mm]
                    + if delta && sm[e][mm] == 0.0 {
                        qm[e][mm] * qm[e][mm]
                    } else {
                        0.0
                    };
            }
            let boost = ((EPS0 + so) / (EPS0 + sd)).sqrt().min(BOOST_MAX);
            for mm in lo..hi {
                gain[e][mm] *= boost;
                qm[e][mm] *= boost;
                sm[e][mm] *= boost;
            }
        }
    }
    // assembling (4.6.18.7.6)
    let smooth = !hdr.smoothing_mode;
    let h_sl = if smooth { 4 } else { 0 };
    if reset {
        for j in 0..4 {
            c.g_hist[j][..m].copy_from_slice(&gain[0][..m]);
            c.q_hist[j][..m].copy_from_slice(&qm[0][..m]);
        }
    }
    let mut idx_noise = c.index_noise;
    let mut idx_sine = c.index_sine;
    for e in 0..ne {
        let transient = e as i32 == c.l_a || e as i32 == c.l_a_prev;
        for i in 2 * c.t_env[e]..2 * c.t_env[e + 1] {
            let xh_t = i + T_HFADJ;
            let mut gf = [0f32; 64];
            let mut qf = [0f32; 64];
            if h_sl > 0 && !transient {
                for mm in 0..m {
                    let mut g = gain[e][mm] * H_SMOOTH[0];
                    let mut q = qm[e][mm] * H_SMOOTH[0];
                    for j in 0..4 {
                        g += c.g_hist[3 - j][mm] * H_SMOOTH[j + 1];
                        q += c.q_hist[3 - j][mm] * H_SMOOTH[j + 1];
                    }
                    gf[mm] = g;
                    qf[mm] = q;
                }
            } else {
                gf[..m].copy_from_slice(&gain[e][..m]);
                qf[..m].copy_from_slice(&qm[e][..m]);
            }
            // shift the smoothing history (newest last)
            for j in 0..3 {
                c.g_hist[j] = c.g_hist[j + 1];
                c.q_hist[j] = c.q_hist[j + 1];
            }
            c.g_hist[3][..m].copy_from_slice(&gain[e][..m]);
            c.q_hist[3][..m].copy_from_slice(&qm[e][..m]);
            let y = &mut c.y[i];
            for mm in 0..m {
                let k = mm + kx;
                let xh = c.xh[k][xh_t];
                let mut v = Cpx::new(xh.re * gf[mm], xh.im * gf[mm]);
                let sv = sm[e][mm];
                if sv != 0.0 {
                    // sinusoid phase phi(f_IndexSine) with (-1)^k on the imaginary part
                    let sgn = if k & 1 == 1 { -1.0 } else { 1.0 };
                    match idx_sine {
                        0 => v.re += sv,
                        1 => v.im += sv * sgn,
                        2 => v.re -= sv,
                        _ => v.im -= sv * sgn,
                    }
                } else if !transient {
                    let n = NOISE[(idx_noise + mm + 1) & 511];
                    v.re += qf[mm] * n[0];
                    v.im += qf[mm] * n[1];
                }
                y[k] = v;
            }
            idx_noise = (idx_noise + m) & 511;
            idx_sine = (idx_sine + 1) & 3;
        }
    }
    c.index_noise = idx_noise;
    c.index_sine = idx_sine;
    for mm in 0..64 {
        c.s_index_prev[mm] = if mm < m { s_idx[ne - 1][mm] } else { false };
    }
    c.prev_num_env = ne;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_final_patch_does_not_synthesize_noise_past_the_limiter() {
        let header = Header {
            start_freq: 2,
            stop_freq: 14,
            freq_scale: 0,
            alter_scale: false,
            noise_bands: 1,
            ..Header::default()
        };
        let bands = freq::compute(&header, 48_000).unwrap();
        assert_eq!(bands.lim, [10, 12, 18]);
        assert_eq!(bands.kx + bands.m, 20);

        let mut channel = Chan::new();
        channel.num_env = 1;
        channel.num_noise = 1;
        channel.t_env[1] = NUM_TIME_SLOTS;
        channel.e_orig[0][..bands.n_low].fill(4096.0);
        channel.q_orig[0][0] = 0.25;
        hf_adjust(&mut channel, &bands, &header, true);

        let has_noise = |sample: &Cpx| sample.re != 0.0 || sample.im != 0.0;
        let output = &channel.y[..2 * NUM_TIME_SLOTS];
        assert!(output.iter().any(|slot| slot[10..18].iter().any(has_noise)));
        assert!(
            output
                .iter()
                .all(|slot| !slot[18..20].iter().any(has_noise))
        );
    }
}
