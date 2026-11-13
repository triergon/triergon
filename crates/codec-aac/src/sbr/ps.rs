//! Parametric Stereo (PS, the HE-AAC v2 tool): `ps_data()` parsing, the parameter state
//! over frames, and the stereo synthesis of the mono QMF matrix (in `ps/synth.rs`).
//!
//! Sources: H. Purnhagen, "Low complexity parametric stereo coding in MPEG-4", DAFx 2004
//! (the mixing matrix of eq. 4-8, the hybrid filterbank, the decorrelator structure and
//! transient reduction); J. Breebaart et al., "Parametric coding of stereo audio", EURASIP
//! JASP 2005:9 (the rotation-and-scale mixing used for ICC modes 3..5); 3GPP TS 26.405
//! V19.0.0 (hybrid prototypes Table 1, the 20-band map Table 2). ISO/IEC 14496-3 is not
//! available: the `ps_data()` syntax, the Huffman books and every table and rule these
//! documents do not print were recovered from the FFmpeg 9.0.2 decoder's output on crafted
//! streams by the scripts in `harness/codecs/aac/ps` (regenerated and checked with
//! `cargo xtask codecs aac-ps-tables`).
//!
//! Syntax: `enable_ps_header` (1); with it `enable_iid` (1) and `iid_mode` (3),
//! `enable_icc` (1) and `icc_mode` (3), `enable_ext` (1). Then `frame_class` (1) and
//! `num_env_idx` (2): class 0 has 0, 1, 2 or 4 envelopes, class 1 has `num_env_idx + 1`,
//! each with a 5-bit border. Per envelope a dt flag and one codeword per band, first for IID
//! (all envelopes), then for ICC. Modes 0..5 give 10, 20, 34, 10, 20, 34 bands, IID modes
//! 3..5 the fine grid; IID modes 6 and 7 are reserved. With `enable_ext`, a 4-bit count (an
//! 8-bit escape at 15) of extension bytes holding 2-bit ids; id 0 carries IPD/OPD: an enable
//! bit, per envelope a dt flag and 5, 11 or 17 IPD codewords (by the last IID mode) and the
//! same for OPD, then one reserved bit.
//!
//! Behaviour over frames, as the reference decoder shows it:
//! - The decoder runs 34 bands when an enabled IID or ICC mode is 2 or 5, else 20; 10 band
//!   values apply to two bands each, 20 band values map onto 34 bands by copying or by the
//!   mean of two indices truncated toward zero.
//! - The matrix entries ramp linearly over each envelope from the previous target to the
//!   envelope's target, reached at its last slot (class 0: slot 32(e + 1)/n - 1; class 1:
//!   the coded border, with one more envelope to slot 31 when the last border is lower). A
//!   frame starts from the target stored at the index of the previous frame's last
//!   envelope; the first PS frame starts from the zero matrix.
//! - A frame without PS data repeats the last PS frame; a header-less frame uses the last
//!   header; a header with IID or ICC disabled resets those values to index 0.
//! - A frame with an invalid value, a decreasing border, a reserved mode or more data than
//!   its extension holds outputs the mono signal on both channels, keeps the signal state
//!   and resets the values; PS stays off until the next header.

mod synth;
mod tables;
#[cfg(test)]
mod tests;

use self::synth::{MAX_PAR, SLOTS, Synth, layouts};
use self::tables as t;
use crate::error::Result;
use crate::fft::Cpx;
use crate::huffman::books;
use std::sync::OnceLock;
use triergon_bitstream::CachedBitReader;
use triergon_codec::{Budget, BudgetVec};

/// Envelopes of a frame, the extra envelope of class 1 included.
const MAX_ENV: usize = 5;
/// IPD/OPD bands by the band count of the last IID mode.
const fn ipd_bands(nb: usize) -> usize {
    match nb {
        10 => 5,
        20 => 11,
        _ => 17,
    }
}
const fn band_count(mode: u8) -> usize {
    match mode % 3 {
        0 => 10,
        1 => 20,
        _ => 34,
    }
}

/// Values of up to four coded envelopes.
type Values = [[i8; MAX_PAR]; 4];
/// The IPD/OPD extension as read: enable bit, per envelope (dt, values) for IPD and OPD.
type PhaseData = (bool, [[(bool, [u8; 17]); 2]; 4]);
/// Mixing matrices per (procedure, grid, IID index + 15, ICC index).
type MatrixTable = [[[[[f32; 4]; 8]; 31]; 2]; 2];
/// Row phases (cos, sin) per parameter band.
type Phases = [(f64, f64); MAX_PAR];

/// The last header's flags and modes (`None`: parameter disabled).
#[derive(Clone, Copy, Debug, Default)]
struct Header {
    iid: Option<u8>,
    icc: Option<u8>,
    ext: bool,
}

impl Header {
    /// 34 bands when an enabled mode is 2 or 5; `None` with IID and ICC disabled.
    fn big(&self) -> Option<bool> {
        if self.iid.is_none() && self.icc.is_none() {
            return None;
        }
        Some(
            [self.iid, self.icc]
                .iter()
                .any(|m| m.is_some_and(|m| m % 3 == 2)),
        )
    }
}

/// The parameters of the last PS frame read without error, on the configuration it runs.
#[derive(Clone, Debug)]
struct Frame {
    envelopes: usize,
    last_slot: [u8; MAX_ENV],
    iid: [[i8; MAX_PAR]; MAX_ENV],
    icc: [[i8; MAX_PAR]; MAX_ENV],
    big: bool,
    fine: bool,
    second: bool,
}

impl Frame {
    fn new() -> Frame {
        Frame {
            envelopes: 0,
            last_slot: [0; MAX_ENV],
            iid: [[0; MAX_PAR]; MAX_ENV],
            icc: [[0; MAX_PAR]; MAX_ENV],
            big: false,
            fine: false,
            second: false,
        }
    }
}

/// One frame's fields as read, before they are resolved against the state.
#[derive(Debug)]
struct Raw {
    header: Option<Header>,
    class1: bool,
    coded_env: usize,
    borders: [u8; 4],
    iid: [(bool, [i8; MAX_PAR]); 4],
    icc: [(bool, [i8; MAX_PAR]); 4],
    /// IPD/OPD extension: enable bit and per envelope (dt, values) for IPD and OPD.
    ipd: Option<PhaseData>,
}

/// What the current frame does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// No PS data was read in this frame.
    Absent,
    /// The PS data was read and runs.
    Run,
    /// The PS does not run: mono output.
    Mono,
}

/// Error frames differ in the envelope count they leave.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// Found while reading: the coded count.
    Read(usize),
    /// The data overran the extension: the count with the extra envelope.
    Length(usize, bool),
}

/// Mixing matrices per (procedure, grid, IID index + 15, ICC index): h11, h12, h21, h22.
fn matrices() -> &'static MatrixTable {
    static M: OnceLock<MatrixTable> = OnceLock::new();
    M.get_or_init(|| {
        let sq2 = std::f64::consts::SQRT_2;
        std::array::from_fn(|second| {
            std::array::from_fn(|fine| {
                std::array::from_fn(|i| {
                    std::array::from_fn(|c| {
                        let idx = i as isize - 15;
                        let grid: &[f64] = if fine == 1 {
                            &t::IID_FINE
                        } else {
                            &t::IID_COARSE
                        };
                        let db = grid
                            .get(idx.unsigned_abs())
                            .map_or(0.0, |&v| v * idx.signum() as f64);
                        let g = 10f64.powf(db / 20.0);
                        let rho = t::ICC[c];
                        let h = if second == 0 {
                            let cl = sq2 * g / (1.0 + g * g).sqrt();
                            let cr = sq2 / (1.0 + g * g).sqrt();
                            let alpha = rho.acos() / 2.0;
                            let beta = alpha * (cr - cl) / sq2;
                            [
                                cl * (beta + alpha).cos(),
                                cl * (beta + alpha).sin(),
                                cr * (beta - alpha).cos(),
                                cr * (beta - alpha).sin(),
                            ]
                        } else {
                            let rho = rho.max(t::ICC_FLOOR_B);
                            let a = if idx == 0 {
                                std::f64::consts::FRAC_PI_4
                            } else {
                                (0.5 * (2.0 * g * rho / (g * g - 1.0)).atan())
                                    .rem_euclid(std::f64::consts::FRAC_PI_2)
                            };
                            let q = g + 1.0 / g;
                            // the reference rounds mu^2 and mu to single precision
                            let mu2 = f64::from((1.0 + (4.0 * rho * rho - 4.0) / (q * q)) as f32);
                            let mu = f64::from(mu2.sqrt() as f32);
                            let gam = ((1.0 - mu) / (1.0 + mu)).sqrt().atan();
                            [
                                sq2 * a.cos() * gam.cos(),
                                -sq2 * a.sin() * gam.sin(),
                                sq2 * a.sin() * gam.cos(),
                                sq2 * a.cos() * gam.sin(),
                            ]
                        };
                        h.map(|v| v as f32)
                    })
                })
            })
        })
    })
}

/// Unit phasors of the IPD/OPD indices (steps of pi/4).
fn phasor(i: u8) -> (f64, f64) {
    const H: f64 = std::f64::consts::FRAC_1_SQRT_2;
    const P: [(f64, f64); 8] = [
        (1.0, 0.0),
        (H, H),
        (0.0, 1.0),
        (-H, H),
        (-1.0, 0.0),
        (-H, -H),
        (0.0, -1.0),
        (H, -H),
    ];
    P[usize::from(i & 7)]
}

/// Coded values of `nb` bands onto the configuration run (`big`: 34 bands).
fn to_config<T: Copy + Into<i32> + TryFrom<i32> + Default>(
    src: &[T],
    nb: usize,
    big: bool,
    out: &mut [T; MAX_PAR],
) {
    *out = [T::default(); MAX_PAR];
    let mut v20 = [T::default(); 20];
    match nb {
        10 => v20
            .iter_mut()
            .enumerate()
            .for_each(|(j, v)| *v = src[j / 2]),
        20 => v20.copy_from_slice(&src[..20]),
        _ => {
            out.copy_from_slice(&src[..MAX_PAR]);
            return;
        }
    }
    if big {
        for (o, &(a, b)) in out.iter_mut().zip(&t::MAP_20_34) {
            let s = v20[a as usize].into() + v20[b as usize].into();
            *o = T::try_from(s / 2).unwrap_or_default(); // truncates toward zero
        }
    } else {
        out[..20].copy_from_slice(&v20);
    }
}

#[derive(Debug)]
pub(crate) struct Ps {
    header: Header,
    /// A header has been read; the PS runs (a header and no error since).
    seen_header: bool,
    active: bool,
    /// Some header enabled IID; the band count of the last IID mode.
    seen_iid: bool,
    iid_bands: usize,
    /// Values of the last envelope read (dt reference and num_env 0): a frame writes the
    /// first entries of its band count, the entries above keep older values.
    last_iid: [i8; MAX_PAR],
    last_icc: [i8; MAX_PAR],
    last_ipd: [[u8; 17]; 2],
    /// IPD/OPD: enabled by the last extension; values per envelope index on the
    /// configuration; smoothing history (two previous envelopes, newest first).
    phase_on: bool,
    phase: [[[u8; MAX_PAR]; 2]; MAX_ENV],
    phase_history: [[[u8; MAX_PAR]; 2]; 2],
    /// Envelope counts of the last two frames read and their configurations (20 bands
    /// before any).
    count: usize,
    count_before: usize,
    big: Option<bool>,
    big_before: Option<bool>,
    frame: Frame,
    have_frame: bool,
    /// Target matrices per envelope index, real and imaginary parts, and the start
    /// matrix of the last frame that ran (read when the frame before counted no envelope).
    store_re: [[[f32; 4]; MAX_PAR]; MAX_ENV],
    store_im: [[[f32; 4]; MAX_PAR]; MAX_ENV],
    start_re: [[f32; 4]; MAX_PAR],
    start_im: [[f32; 4]; MAX_PAR],
    status: Status,
    synth: Synth,
}

impl Ps {
    pub(crate) fn new(budget: &Budget) -> Result<Ps> {
        Ok(Ps {
            header: Header::default(),
            seen_header: false,
            active: false,
            seen_iid: false,
            iid_bands: 10,
            last_iid: [0; MAX_PAR],
            last_icc: [0; MAX_PAR],
            last_ipd: [[0; 17]; 2],
            phase_on: false,
            phase: [[[0; MAX_PAR]; 2]; MAX_ENV],
            phase_history: [[[0; MAX_PAR]; 2]; 2],
            count: 0,
            count_before: 0,
            big: Some(false),
            big_before: None,
            frame: Frame::new(),
            have_frame: false,
            store_re: [[[0.0; 4]; MAX_PAR]; MAX_ENV],
            store_im: [[[0.0; 4]; MAX_PAR]; MAX_ENV],
            start_re: [[0.0; 4]; MAX_PAR],
            start_im: [[0.0; 4]; MAX_PAR],
            status: Status::Absent,
            synth: Synth::new(budget)?,
        })
    }

    /// Drop the signal history and the stored matrices (after a discontinuity); the next
    /// PS frame starts as the first one of a stream.
    pub(crate) fn reset(&mut self) {
        self.synth.reset();
        self.store_re = [[[0.0; 4]; MAX_PAR]; MAX_ENV];
        self.store_im = [[[0.0; 4]; MAX_PAR]; MAX_ENV];
        self.start_re = [[0.0; 4]; MAX_PAR];
        self.start_im = [[0.0; 4]; MAX_PAR];
        self.phase_history = [[[0; MAX_PAR]; 2]; 2];
        self.count = 0;
        self.count_before = 0;
        self.big = Some(false);
        self.big_before = None;
        self.have_frame = false;
        self.status = Status::Absent;
    }

    /// Return to the state of `new`.
    pub(crate) fn renew(&mut self) {
        self.reset();
        self.header = Header::default();
        self.seen_header = false;
        self.active = false;
        self.seen_iid = false;
        self.iid_bands = 10;
        self.last_iid = [0; MAX_PAR];
        self.last_icc = [0; MAX_PAR];
        self.last_ipd = [[0; 17]; 2];
        self.phase_on = false;
        self.phase = [[[0; MAX_PAR]; 2]; MAX_ENV];
        self.frame = Frame::new();
    }

    /// Read one PS extension of `bits_left` bits; returns the bits it takes.
    pub(crate) fn parse(
        &mut self,
        br: &mut CachedBitReader<'_>,
        bits_left: usize,
    ) -> Result<usize> {
        let start = br.position();
        let raw = self.read(br);
        let used = br.position() - start;
        let taken = match raw {
            None => {
                // reserved mode: nothing after the header is read
                self.seen_header = true;
                self.fault(None);
                bits_left
            }
            Some(raw) if raw.header.is_none() && !self.active => {
                if self.seen_header {
                    self.read_only(&raw, used > bits_left);
                }
                // before any header the extension is skipped
                bits_left
            }
            Some(raw) => {
                if self.apply_raw(&raw, used > bits_left) {
                    used.min(bits_left)
                } else {
                    bits_left
                }
            }
        };
        br.seek(start + taken);
        Ok(taken)
    }

    /// Read the fields of `ps_data()`; `None` for a reserved mode.
    fn read(&self, br: &mut CachedBitReader<'_>) -> Option<Raw> {
        let header = if br.bit() {
            let mut h = Header::default();
            if br.bit() {
                h.iid = Some(br.read(3) as u8);
            }
            if br.bit() {
                h.icc = Some(br.read(3) as u8);
            }
            h.ext = br.bit();
            if h.iid.is_some_and(|m| m > 5) || h.icc.is_some_and(|m| m > 5) {
                return None;
            }
            Some(h)
        } else {
            None
        };
        let h = header.unwrap_or(self.header);
        let iid_bands = h.iid.map_or(self.iid_bands, band_count);
        let class1 = br.bit();
        let idx = br.read(2) as usize;
        let coded_env = if class1 { idx + 1 } else { [0, 1, 2, 4][idx] };
        let mut raw = Raw {
            header,
            class1,
            coded_env,
            borders: [0; 4],
            iid: [(false, [0; MAX_PAR]); 4],
            icc: [(false, [0; MAX_PAR]); 4],
            ipd: None,
        };
        if class1 {
            for b in &mut raw.borders[..coded_env] {
                *b = br.read(5) as u8;
            }
        }
        let b = &books().ps;
        // books: IID df/dt, fine IID df/dt, ICC df/dt; symbols are values + 14, 30 or 7
        if let Some(mode) = h.iid {
            let (book, off) = if mode >= 3 { (2, 30) } else { (0, 14) };
            Self::read_values(
                br,
                &mut raw.iid[..coded_env],
                band_count(mode),
                [&b[book], &b[book + 1]],
                off,
            );
        }
        if let Some(mode) = h.icc {
            Self::read_values(
                br,
                &mut raw.icc[..coded_env],
                band_count(mode),
                [&b[4], &b[5]],
                7,
            );
        }
        if h.ext {
            let mut cnt = br.read(4) as isize;
            if cnt == 15 {
                cnt += br.read(8) as isize;
            }
            let mut left = 8 * cnt;
            while left > 7 {
                let id = br.read(2);
                left -= 2;
                let before = br.position();
                if id == 0 {
                    let nb = ipd_bands(iid_bands);
                    let on = br.bit();
                    let mut env = [[(false, [0u8; 17]); 2]; 4];
                    if on {
                        for e in env.iter_mut().take(coded_env) {
                            for (k, (dt, vals)) in e.iter_mut().enumerate() {
                                *dt = br.bit();
                                let vlc = &b[6 + 2 * k + usize::from(*dt)];
                                for v in &mut vals[..nb] {
                                    *v = vlc.decode(br).unwrap_or(0) as u8;
                                }
                            }
                        }
                    }
                    br.skip(1);
                    raw.ipd = Some((on, env));
                    left -= (br.position() - before) as isize;
                } else {
                    br.skip(left as usize);
                    left = 0;
                }
            }
            if left > 0 {
                br.skip(left as usize);
            }
        }
        Some(raw)
    }

    /// Per envelope a dt flag and `nb` codewords of the df or dt book.
    fn read_values(
        br: &mut CachedBitReader<'_>,
        out: &mut [(bool, [i8; MAX_PAR])],
        nb: usize,
        book: [&crate::huffman::Vlc; 2],
        offset: i32,
    ) {
        for (dt, vals) in out {
            *dt = br.bit();
            let vlc = book[usize::from(*dt)];
            for v in &mut vals[..nb] {
                // complete prefix codes: decoding fails only past the end of the data
                *v = (vlc.decode(br).map_or(0, |s| s as i32 - offset)) as i8;
            }
        }
    }

    /// The last slot of every envelope (class 1: one more envelope to slot 31 when the last
    /// border is lower); `None` when the borders decrease.
    fn borders(raw: &Raw) -> Option<([u8; MAX_ENV], usize)> {
        let mut out = [0u8; MAX_ENV];
        let n = raw.coded_env;
        if n == 0 {
            out[0] = 31;
            return Some((out, 1));
        }
        if !raw.class1 {
            for (e, o) in out.iter_mut().enumerate().take(n) {
                *o = (SLOTS * (e + 1) / n - 1) as u8;
            }
            return Some((out, n));
        }
        out[..n].copy_from_slice(&raw.borders[..n]);
        if out[..n].windows(2).any(|w| w[1] < w[0]) {
            return None;
        }
        if out[n - 1] < 31 {
            out[n] = 31;
            return Some((out, n + 1));
        }
        Some((out, n))
    }

    /// Resolve dt/df coded values against `last`; `None` when a value leaves `lo..=hi`.
    fn resolve(
        coded: &[(bool, [i8; MAX_PAR]); 4],
        n: usize,
        nb: usize,
        last: &[i8; MAX_PAR],
        lo: i8,
        hi: i8,
    ) -> Option<Values> {
        let mut out = [[0i8; MAX_PAR]; 4];
        for e in 0..n {
            let (dt, d) = &coded[e];
            let mut prev = 0i16;
            for b in 0..nb {
                let base = if *dt {
                    i16::from(if e == 0 { last[b] } else { out[e - 1][b] })
                } else {
                    prev
                };
                let v = base + i16::from(d[b]);
                if v < i16::from(lo) || v > i16::from(hi) {
                    return None;
                }
                out[e][b] = v as i8;
                prev = v;
            }
        }
        Some(out)
    }

    /// The configuration of a frame with header `h`: a header with IID and ICC disabled
    /// keeps the previous one (20 bands at the start).
    fn config(&self, h: Header) -> bool {
        h.big().unwrap_or(self.big.unwrap_or(false))
    }

    /// A frame of configuration `big` was read to its end.
    fn read_config(&mut self, big: bool) {
        self.big_before = self.big;
        self.big = Some(big);
    }

    /// Keep the last envelope's values: the first entries of the frame's band counts.
    fn keep(&mut self, h: Header, iid: &[i8; MAX_PAR], icc: &[i8; MAX_PAR]) {
        if let Some(m) = h.iid {
            let n = band_count(m);
            self.last_iid[..n].copy_from_slice(&iid[..n]);
        }
        if let Some(m) = h.icc {
            let n = band_count(m);
            self.last_icc[..n].copy_from_slice(&icc[..n]);
        }
    }

    /// An error frame: mono output, values reset, PS off until the next header.
    fn fault(&mut self, fault: Option<Fault>) {
        match fault {
            Some(Fault::Read(n)) => {
                self.count_before = self.count;
                self.count = n;
            }
            Some(Fault::Length(n, big)) => {
                self.count_before = self.count;
                self.count = n;
                self.read_config(big);
            }
            None => {}
        }
        self.last_iid = [0; MAX_PAR];
        self.last_icc = [0; MAX_PAR];
        self.active = false;
        self.status = Status::Mono;
    }

    /// The values of envelopes read with the current modes, or `None` on an invalid value.
    fn values(&self, raw: &Raw, h: Header) -> Option<(Values, Values)> {
        let n = raw.coded_env;
        let mut iid = [[0; MAX_PAR]; 4];
        let mut icc = [[0; MAX_PAR]; 4];
        if let Some(m) = h.iid {
            let lim = if m >= 3 { 15 } else { 7 };
            iid = Self::resolve(&raw.iid, n, band_count(m), &self.last_iid, -lim, lim)?;
        }
        if let Some(m) = h.icc {
            icc = Self::resolve(&raw.icc, n, band_count(m), &self.last_icc, 0, 7)?;
        }
        Some((iid, icc))
    }

    /// A header-less frame read while the PS is off after an error: its count, values and
    /// configuration count, nothing is applied; more data than its extension holds makes it
    /// an error frame.
    fn read_only(&mut self, raw: &Raw, overrun: bool) {
        let Some((_, n)) = Self::borders(raw) else {
            self.fault(Some(Fault::Read(raw.coded_env)));
            return;
        };
        if raw.coded_env > 0 {
            let Some((iid, icc)) = self.values(raw, self.header) else {
                self.fault(Some(Fault::Read(raw.coded_env)));
                return;
            };
            if !overrun {
                self.keep(
                    self.header,
                    &iid[raw.coded_env - 1],
                    &icc[raw.coded_env - 1],
                );
            }
        }
        if overrun {
            self.fault(Some(Fault::Length(n, self.config(self.header))));
            return;
        }
        self.count_before = self.count;
        self.count = n;
        self.read_config(self.config(self.header));
        self.status = Status::Mono;
    }

    /// Resolve a frame read with a header or while the PS runs; false for an error frame.
    fn apply_raw(&mut self, raw: &Raw, overrun: bool) -> bool {
        if let Some(h) = raw.header {
            self.seen_header = true;
            self.header = h;
            if let Some(m) = h.iid {
                self.seen_iid = true;
                self.iid_bands = band_count(m);
            }
        }
        let h = self.header;
        let Some((last_slot, envelopes)) = Self::borders(raw) else {
            self.fault(Some(Fault::Read(raw.coded_env)));
            return false;
        };
        let big = self.config(h);
        if h.iid.is_none() {
            self.last_iid = [0; MAX_PAR];
        }
        if h.icc.is_none() {
            self.last_icc = [0; MAX_PAR];
        }
        let n = raw.coded_env;
        let (iid, icc) = if n == 0 {
            // one envelope holding the last values; kept fine values on the coarse grid fail
            let kept = &self.last_iid[..h.iid.map_or(0, band_count)];
            if h.iid.is_some_and(|m| m < 3) && kept.iter().any(|v| v.abs() > 7) {
                self.fault(Some(Fault::Read(n)));
                return false;
            }
            let mut a = [[0; MAX_PAR]; 4];
            let mut c = [[0; MAX_PAR]; 4];
            a[0] = self.last_iid;
            c[0] = self.last_icc;
            (a, c)
        } else {
            let Some(v) = self.values(raw, h) else {
                self.fault(Some(Fault::Read(n)));
                return false;
            };
            v
        };
        if overrun {
            self.fault(Some(Fault::Length(envelopes, big)));
            return false;
        }
        let coded = n.max(1);
        let f = &mut self.frame;
        f.envelopes = envelopes;
        f.last_slot = last_slot;
        f.big = big;
        f.fine = h.iid.is_some_and(|m| m >= 3);
        f.second = h.icc.is_some_and(|m| m >= 3);
        for e in 0..envelopes {
            let src = e.min(coded - 1);
            f.iid[e] = [0; MAX_PAR];
            f.icc[e] = [0; MAX_PAR];
            if let Some(m) = h.iid {
                to_config(&iid[src], band_count(m), big, &mut f.iid[e]);
            }
            if let Some(m) = h.icc {
                to_config(&icc[src], band_count(m), big, &mut f.icc[e]);
            }
        }
        self.keep(h, &iid[coded - 1], &icc[coded - 1]);
        if let Some((on, env)) = &raw.ipd {
            self.phase_on = *on;
            if *on {
                let nb = ipd_bands(self.iid_bands);
                let mut vals = [[[0u8; 17]; 2]; 4];
                for e in 0..n {
                    for k in 0..2 {
                        let (dt, d) = &env[e][k];
                        let mut prev = 0u8;
                        for b in 0..nb {
                            let base = match (*dt, e) {
                                (false, _) => prev,
                                (true, 0) => self.last_ipd[k][b],
                                (true, _) => vals[e - 1][k][b],
                            };
                            vals[e][k][b] = (base + d[b]) & 7;
                            prev = vals[e][k][b];
                        }
                        let mut full = [0u8; MAX_PAR];
                        full[..17].copy_from_slice(&vals[e][k]);
                        if self.iid_bands == 34 && !big {
                            // not observed: IID last ran 34 bands, the frame runs 20
                            self.phase[e][k] = [0; MAX_PAR];
                            self.phase[e][k][..20].copy_from_slice(&full[..20]);
                        } else {
                            to_config(&full, self.iid_bands, big, &mut self.phase[e][k]);
                        }
                    }
                }
                if n > 0 {
                    self.last_ipd = vals[n - 1];
                }
                for e in n.max(1)..envelopes {
                    self.phase[e] = self.phase[e - 1];
                }
            } else {
                self.phase = [[[0; MAX_PAR]; 2]; MAX_ENV];
                self.last_ipd = [[0; 17]; 2];
            }
        }
        self.count_before = self.count;
        self.count = envelopes;
        self.read_config(big);
        self.have_frame = true;
        self.active = true;
        self.status = Status::Run;
        true
    }

    /// Run the PS on one frame: `x` (38 slots) becomes the left channel, `x_r` the right.
    pub(crate) fn apply(
        &mut self,
        x: &mut BudgetVec<[Cpx; 64]>,
        x_r: &mut BudgetVec<[Cpx; 64]>,
        _ready: bool,
    ) {
        let status = std::mem::replace(&mut self.status, Status::Absent);
        let run = match status {
            Status::Run => true,
            Status::Absent => self.active && self.have_frame,
            Status::Mono => false,
        };
        if !run {
            for i in 0..SLOTS {
                x_r[i] = x[i];
            }
            return;
        }
        // a frame whose configuration differs from that of the frame read before it (or a
        // repeat of it) restarts the decorrelator and the ducking
        if self.changed() {
            self.synth.restart();
        }
        self.matrices();
        let big = self.frame.big;
        self.synth.run(big, &mut x[..], &mut x_r[..]);
    }

    fn changed(&self) -> bool {
        matches!((self.big_before, self.big), (Some(a), Some(b)) if a != b)
    }

    /// The frame's per-slot matrices into the synthesis scratch.
    fn matrices(&mut self) {
        let f = self.frame.clone();
        let pars = layouts()[usize::from(f.big)].pars;
        let image_pars = layouts()[usize::from(f.big)].image_pars;
        let table = matrices();
        let has_phase = self.phase_on && self.seen_iid;
        // start of the first envelope: the target stored at the previous frame's last index,
        // or the last start when that frame counted no envelope (zero before any frame ran)
        let (mut re0, mut im0) = match self.count_before.checked_sub(1) {
            Some(s) => (self.store_re[s], self.store_im[s]),
            None => (self.start_re, self.start_im),
        };
        if self.changed() {
            let old = self.big_before == Some(true);
            convert(&mut re0, old);
            convert(&mut im0, old);
        }
        self.start_re = re0;
        self.start_im = im0;
        let mut first = 0usize;
        for e in 0..f.envelopes {
            if e > 0 {
                re0 = self.store_re[e - 1];
                im0 = self.store_im[e - 1];
            }
            let mut re1 = [[0f32; 4]; MAX_PAR];
            let mut im1 = [[0f32; 4]; MAX_PAR];
            let (pl, pr) = if has_phase {
                self.smooth(e, pars)
            } else {
                ([(1.0, 0.0); MAX_PAR], [(1.0, 0.0); MAX_PAR])
            };
            for b in 0..pars {
                let h = table[usize::from(f.second)][usize::from(f.fine)]
                    [(i32::from(f.iid[e][b]) + 15) as usize][f.icc[e][b] as usize];
                for (i, &v) in h.iter().enumerate() {
                    let (c, s) = if i < 2 { pl[b] } else { pr[b] };
                    re1[b][i] = (f64::from(v) * c) as f32;
                    im1[b][i] = (f64::from(v) * s) as f32;
                }
            }
            self.store_re[e][..pars].copy_from_slice(&re1[..pars]);
            if has_phase {
                self.store_im[e][..pars].copy_from_slice(&im1[..pars]);
            }
            let last = usize::from(f.last_slot[e]);
            if last + 1 > first {
                let width = (last + 1 - first) as f32;
                for slot in first..=last {
                    let w = (slot + 1 - first) as f32 / width;
                    let row = &mut self.synth.matrix[slot];
                    for b in 0..pars {
                        let m = &mut row[b];
                        for i in 0..4 {
                            let (r0, r1) = (re0[b][i], re1[b][i]);
                            m.re[i] = r0 + (r1 - r0) * w;
                            if has_phase {
                                let (i0, i1) = (im0[b][i], im1[b][i]);
                                m.im[i] = i0 + (i1 - i0) * w;
                                if image_pars[b] {
                                    m.image_im[i] = -i0 + (i1 + i0) * w;
                                }
                            } else {
                                m.im[i] = 0.0;
                                m.image_im[i] = 0.0;
                            }
                        }
                    }
                }
                first = last + 1;
            }
        }
        self.synth.complex = has_phase;
    }

    /// Smoothed row phases (cos, sin) of envelope `e`; advances the smoothing history.
    fn smooth(&mut self, e: usize, pars: usize) -> (Phases, Phases) {
        let mut out = [[(1.0, 0.0); MAX_PAR]; 2];
        let mut unit = [[(1.0, 0.0); MAX_PAR]; 2];
        for k in 0..2 {
            for b in 0..pars {
                let w = &t::PHASE_WEIGHTS;
                let p = [
                    phasor(self.phase[e][k][b]),
                    phasor(self.phase_history[0][k][b]),
                    phasor(self.phase_history[1][k][b]),
                ];
                let re: f64 = p.iter().zip(w).map(|(p, w)| p.0 * w).sum();
                let im: f64 = p.iter().zip(w).map(|(p, w)| p.1 * w).sum();
                // the phase of the weighted sum (it is never zero: |4u0| > |2u1| + |u2|)
                let n = re.hypot(im);
                unit[k][b] = (re / n, im / n);
            }
            self.phase_history[1][k] = self.phase_history[0][k];
            self.phase_history[0][k] = self.phase[e][k];
        }
        // row 1 turns by the OPD, row 2 by OPD - IPD
        let (ipd, opd) = (unit[0], unit[1]);
        for b in 0..pars {
            out[0][b] = opd[b];
            out[1][b] = (
                opd[b].0 * ipd[b].0 + opd[b].1 * ipd[b].1,
                opd[b].1 * ipd[b].0 - opd[b].0 * ipd[b].1,
            );
        }
        (out[0], out[1])
    }
}

/// Stored rows of the other configuration's layout read as start matrices: the mean of
/// the listed rows, entry by entry.
fn convert(rows: &mut [[f32; 4]; MAX_PAR], old_big: bool) {
    let src = *rows;
    let mean = |list: &[u8]| {
        let mut acc = [0f32; 4];
        for &r in list {
            for (a, v) in acc.iter_mut().zip(&src[usize::from(r)]) {
                *a += v;
            }
        }
        acc.map(|v| v / list.len() as f32)
    };
    if old_big {
        for (row, list) in rows.iter_mut().zip(&t::CONV_34_20) {
            *row = mean(list);
        }
    } else {
        for (row, &(a, b)) in rows.iter_mut().zip(&t::MAP_20_34) {
            *row = mean(&[a, b]);
        }
    }
}
