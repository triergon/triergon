//! Cosine-modulated synthesis filter banks.
//!
//! ETSI TS 102 114 clause C.3.6 gives the 32-band synthesis as a cosine modulation
//! followed by a polyphase window over the sign-modified 512-tap prototype of Annex D.8.
//! The modulation stage of C.3.6 equals `V = 0.25 * DCT-IV(X)` with
//! `V[n] = sum_k X[k] cos((2k+1)(2n+1) pi / (4 N))` (N = 32), after which every input
//! block contributes `U[i] = V[i] - V[N-1-i]` (even taps) and `W[i] = -(V[i] + V[N-1-i])`
//! (odd taps) to 16 consecutive output blocks:
//! `out_s[i] = sum_{m=0}^{15} h[N m + i] * (m even ? U_{s-m}[i] : W_{s-m}[i])`.
//! The X96 64-band bank (clause 6.2.4.7, Annex D.9) uses the same structure with N = 64
//! and the 1 024-tap prototype.

use crate::dct::Dct4;

pub(crate) struct Qmf {
    n: usize,
    /// Sign-modified prototype regrouped as 16 taps x n, gain folded in.
    coef: Vec<f32>,
    dct: Dct4,
}

/// Per-channel synthesis history: 16 slots of `[U | W]` vectors (2 n floats each).
pub(crate) struct QmfState {
    n: usize,
    ring: Vec<f32>,
    pos: usize,
}

impl QmfState {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            n,
            ring: vec![0.0; 16 * 2 * n],
            pos: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.ring.iter_mut().for_each(|v| *v = 0.0);
        self.pos = 0;
    }
}

impl Qmf {
    pub(crate) fn new(proto: &[f64], scale: f64) -> Self {
        let n = proto.len() / 16;
        let coef = proto.iter().map(|&c| (c * scale) as f32).collect();
        Self {
            n,
            coef,
            dct: Dct4::new(n),
        }
    }

    /// Synthesize one block: `input` holds `n` subband samples (clobbered), `out`
    /// receives `n` PCM samples.
    #[inline]
    pub(crate) fn synth(&self, st: &mut QmfState, input: &mut [f32], out: &mut [f32]) {
        match self.n {
            32 => self.synth_n::<32>(st, input, out),
            _ => self.synth_n::<64>(st, input, out),
        }
    }

    #[inline]
    fn synth_n<const N: usize>(&self, st: &mut QmfState, input: &mut [f32], out: &mut [f32]) {
        debug_assert_eq!(st.n, N);
        let x: &mut [f32; N] = (&mut input[..N]).try_into().expect("block length");
        // V = DCT-IV(X); the 0.25 factor of C.3.6 is folded into the prototype.
        self.dct.transform(x);
        // The new slot (time s) replaces the oldest one; slot (pos + m) & 15 holds s - m.
        st.pos = (st.pos + 15) & 15;
        let base = st.pos * 2 * N;
        let (u, w) = st.ring[base..base + 2 * N].split_at_mut(N);
        let u: &mut [f32; N] = u.try_into().expect("slot");
        let w: &mut [f32; N] = w.try_into().expect("slot");
        for i in 0..N {
            let a = x[i];
            let b = x[N - 1 - i];
            u[i] = a - b;
            w[i] = -(a + b);
        }
        let out: &mut [f32; N] = (&mut out[..N]).try_into().expect("block length");
        let mut acc = [0f32; N];
        let coef = &self.coef[..16 * N];
        for m in 0..16 {
            let idx = (st.pos + m) & 15;
            let off = idx * 2 * N + if m & 1 == 0 { 0 } else { N };
            let g: &[f32; N] = st.ring[off..off + N].try_into().expect("slot");
            let h: &[f32; N] = coef[m * N..m * N + N].try_into().expect("taps");
            for i in 0..N {
                acc[i] += h[i] * g[i];
            }
        }
        *out = acc;
    }
}

#[cfg(test)]
mod tests {
    use super::{Qmf, QmfState};

    /// Timing probe (development only): `cargo test --release -- --ignored qmf_speed --nocapture`.
    #[test]
    #[ignore = "timing probe; run on demand with --release"]
    fn qmf_speed() {
        for (proto, n) in [
            (&crate::tables::QMF_NONPERFECT[..], 32usize),
            (&crate::tables::X96_FIR[..], 64),
        ] {
            let q = Qmf::new(proto, 1.0);
            let mut st = QmfState::new(n);
            let mut x = vec![0.0f32; n];
            let mut out = vec![0.0f32; n];
            let iters = 2_000_000;
            let t = std::time::Instant::now();
            let mut acc = 0.0f32;
            for i in 0..iters {
                x.iter_mut()
                    .enumerate()
                    .for_each(|(k, v)| *v = ((i + k) % 7) as f32 - 3.0);
                q.synth(&mut st, &mut x, &mut out);
                acc += out[3];
            }
            let ns = t.elapsed().as_nanos() as f64 / iters as f64;
            eprintln!(
                "n={n}: {ns:.1} ns per block ({:.2} ns/sample) acc={acc}",
                ns / n as f64
            );
        }
    }
}
