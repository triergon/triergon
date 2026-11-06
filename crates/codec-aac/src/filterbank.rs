//! Filterbank: block switching, sine/KBD windows, IMDCT and overlap-add
//! (ISO/IEC 14496-3 4.6.11), plus the forward MDCT used by LTP.

use crate::fft::Mdct;
use crate::ics::{EIGHT_SHORT, LONG_START, LONG_STOP};

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let h = x / 2.0;
    for k in 1..60 {
        term *= h / k as f64;
        sum += term * term;
    }
    sum
}

/// Rising half (n < N/2) of the KBD window of length `n2 * 2` (ISO/IEC 14496-3 4.6.11.3.2).
pub(crate) fn kbd_window(n2: usize, alpha: f64) -> Vec<f32> {
    let n = n2 * 2;
    let q = n as f64 / 4.0;
    let w: Vec<f64> = (0..=n2)
        .map(|p| {
            let r = (p as f64 - q) / q;
            bessel_i0(std::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = w.iter().sum();
    let mut acc = 0.0;
    (0..n2)
        .map(|i| {
            acc += w[i];
            (acc / total).sqrt() as f32
        })
        .collect()
}

/// Rising half of the sine window of length `n2 * 2`.
pub(crate) fn sine_window(n2: usize) -> Vec<f32> {
    let n = (n2 * 2) as f64;
    (0..n2)
        .map(|i| (std::f64::consts::PI / n * (i as f64 + 0.5)).sin() as f32)
        .collect()
}

#[derive(Debug)]
pub(crate) struct Windows {
    /// [shape][n], rising halves
    pub long: [Vec<f32>; 2],
    pub short: [Vec<f32>; 2],
    /// Effective 1024-sample window of the first half of a long block, [shape of the
    /// previous frame]: ONLY_LONG / LONG_START, and LONG_STOP (zeros, short rise, ones).
    rise_long: [Vec<f32>; 2],
    rise_stop: [Vec<f32>; 2],
    /// Same for the second half (time order), [current shape]: ONLY_LONG / LONG_STOP, and
    /// LONG_START (ones, short fall, zeros).
    fall_long: [Vec<f32>; 2],
    fall_start: [Vec<f32>; 2],
}

impl Windows {
    pub(crate) fn new() -> Windows {
        let long = [sine_window(1024), kbd_window(1024, 4.0)];
        let short = [sine_window(128), kbd_window(128, 6.0)];
        let stop = |ws: &[f32]| {
            (0..1024)
                .map(|n| {
                    if n < 448 {
                        0.0
                    } else if n < 576 {
                        ws[n - 448]
                    } else {
                        1.0
                    }
                })
                .collect()
        };
        let start = |ws: &[f32]| {
            (0..1024)
                .map(|n| {
                    if n < 448 {
                        1.0
                    } else if n < 576 {
                        ws[575 - n]
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        let fall = |wl: &[f32]| wl.iter().rev().copied().collect();
        Windows {
            rise_long: long.clone(),
            rise_stop: [stop(&short[0]), stop(&short[1])],
            fall_long: [fall(&long[0]), fall(&long[1])],
            fall_start: [start(&short[0]), start(&short[1])],
            long,
            short,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Filterbank {
    pub win: Windows,
    imdct_long: Mdct,
    imdct_short: Mdct,
    mdct_long: Mdct,
    buf: Vec<f32>,
    tmp: Vec<f32>,
}

/// Output scaling: FFmpeg's float output is the ISO 16-bit PCM scale divided by 32768.
pub(crate) const OUT_SCALE: f64 = 1.0 / 32768.0;

impl Filterbank {
    pub(crate) fn new() -> Filterbank {
        Filterbank {
            win: Windows::new(),
            imdct_long: Mdct::new(2048, 2.0 / 2048.0 * OUT_SCALE),
            imdct_short: Mdct::new(256, 2.0 / 256.0 * OUT_SCALE),
            mdct_long: Mdct::new(2048, 2.0 * 32768.0),
            buf: vec![0.0; 2048],
            tmp: vec![0.0; 256],
        }
    }

    /// Inverse transform + windowing of one frame. `out` receives 1024 samples
    /// (overlap + new first half), `overlap` is replaced by the new second half.
    /// If `full` is given it receives the complete windowed 2048-sample block (for LTP).
    pub(crate) fn synth(
        &mut self,
        coef: &[f32],
        seq: u8,
        shape: u8,
        prev_shape: u8,
        overlap: &mut [f32],
        out: &mut [f32],
    ) {
        if seq != EIGHT_SHORT {
            let w = &self.win;
            let rise = if seq == LONG_STOP {
                &w.rise_stop
            } else {
                &w.rise_long
            }[prev_shape as usize]
                .as_slice();
            let fall = if seq == LONG_START {
                &w.fall_start
            } else {
                &w.fall_long
            }[shape as usize]
                .as_slice();
            // IMDCT output in terms of the DCT-IV result y = (ye, yo) (see Mdct::imdct_split),
            // windowed and overlap-added in one pass. With y[2k] = ye[k], y[1023 - 2k] = yo[k]:
            // out[n] = ovl[n] + y[512 + n] r[n] (n < 512), ovl[n] - y[1535 - n] r[n] (n < 1024);
            // new ovl[n] = -y[511 - n] f[n] (n < 512), -y[n - 512] f[n] (n < 1024). Each quarter
            // interleaves one ascending and one descending ye / yo run.
            let (ye, yo) = self.imdct_long.imdct_split(coef);
            let (out, overlap, rise, fall) = (
                &mut out[..1024],
                &mut overlap[..1024],
                &rise[..1024],
                &fall[..1024],
            );
            let (ye0, ye1) = ye.split_at(256);
            let (yo0, yo1) = yo.split_at(256);
            let (out0, out1) = out.split_at_mut(512);
            let (ov0, ov1) = overlap.split_at_mut(512);
            for ((((o, v), r), &e), &d) in out0
                .chunks_exact_mut(2)
                .zip(ov0.chunks_exact(2))
                .zip(rise[..512].chunks_exact(2))
                .zip(ye1)
                .zip(yo0.iter().rev())
            {
                o[0] = v[0] + e * r[0];
                o[1] = v[1] + d * r[1];
            }
            for ((((o, v), r), &d), &e) in out1
                .chunks_exact_mut(2)
                .zip(ov1.chunks_exact(2))
                .zip(rise[512..].chunks_exact(2))
                .zip(yo0)
                .zip(ye1.iter().rev())
            {
                o[0] = v[0] - d * r[0];
                o[1] = v[1] - e * r[1];
            }
            for (((v, f), &d), &e) in ov0
                .chunks_exact_mut(2)
                .zip(fall[..512].chunks_exact(2))
                .zip(yo1)
                .zip(ye0.iter().rev())
            {
                v[0] = -d * f[0];
                v[1] = -e * f[1];
            }
            for (((v, f), &e), &d) in ov1
                .chunks_exact_mut(2)
                .zip(fall[512..].chunks_exact(2))
                .zip(ye0)
                .zip(yo1.iter().rev())
            {
                v[0] = -e * f[0];
                v[1] = -d * f[1];
            }
            return;
        }
        let buf = &mut self.buf;
        let ws_prev = &self.win.short[prev_shape as usize];
        let ws_cur = &self.win.short[shape as usize];
        for v in buf.iter_mut() {
            *v = 0.0;
        }
        for w in 0..8 {
            self.imdct_short
                .imdct(&coef[w * 128..w * 128 + 128], &mut self.tmp);
            let wr = if w == 0 { ws_prev } else { ws_cur };
            let base = 448 + 128 * w;
            for i in 0..128 {
                buf[base + i] += self.tmp[i] * wr[i];
                buf[base + 128 + i] += self.tmp[128 + i] * ws_cur[127 - i];
            }
        }
        for i in 0..1024 {
            out[i] = overlap[i] + buf[i];
        }
        overlap[..1024].copy_from_slice(&buf[1024..2048]);
    }

    /// Forward MDCT (LTP, ISO/IEC 14496-3 4.6.6): windowing of a 2048-sample time block
    /// according to the window sequence/shapes, then MDCT (scaled to the spectral domain).
    pub(crate) fn mdct_windowed(
        &mut self,
        time: &[f32],
        seq: u8,
        shape: u8,
        prev_shape: u8,
        out: &mut [f32],
    ) {
        let wl_prev = &self.win.long[prev_shape as usize];
        let wl_cur = &self.win.long[shape as usize];
        let ws_prev = &self.win.short[prev_shape as usize];
        let ws_cur = &self.win.short[shape as usize];
        let buf = &mut self.buf;
        buf[..2048].copy_from_slice(&time[..2048]);
        match seq {
            LONG_STOP => {
                for v in &mut buf[0..448] {
                    *v = 0.0;
                }
                for i in 0..128 {
                    buf[448 + i] *= ws_prev[i];
                }
            }
            _ => {
                for i in 0..1024 {
                    buf[i] *= wl_prev[i];
                }
            }
        }
        match seq {
            LONG_START => {
                for i in 0..128 {
                    buf[1472 + i] *= ws_cur[127 - i];
                }
                for v in &mut buf[1600..2048] {
                    *v = 0.0;
                }
            }
            _ => {
                for i in 0..1024 {
                    buf[1024 + i] *= wl_cur[1023 - i];
                }
            }
        }
        let b: &[f32] = &self.buf;
        // MDCT of a windowed block; the spec forward MDCT has no extra factor, our time
        // samples are float-scaled (1/32768) so the transform scale restores the spectral domain.
        self.mdct_long.mdct(b, out);
    }
}
