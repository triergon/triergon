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

/// Output pairs `(i, N-1-i)` computed together by the polyphase window.
const GROUP: usize = 8;

/// Synthesis bank of `TAPS / 16` bands.
#[derive(Debug)]
pub(crate) struct Qmf<const TAPS: usize> {
    /// Sign-modified prototype, gain folded in, in the order the window reads it: for
    /// each group of `GROUP` outputs `i` below `n / 2` and each tap `m`, the coefficients
    /// `h[n m + i]`, then `h[n m + n - 1 - i]` of the mirrored outputs.
    coef: [f32; TAPS],
    dct: Dct4,
}

/// Per-channel synthesis history: 16 slots of `[U | W]` half vectors (`n` floats each,
/// `LEN = 16 n`). `U` is antisymmetric and `W` symmetric about the middle, so their lower
/// halves determine them.
#[derive(Clone, Debug)]
pub(crate) struct QmfState<const LEN: usize> {
    ring: [f32; LEN],
    pos: usize,
}

impl<const LEN: usize> QmfState<LEN> {
    pub(crate) const fn new() -> Self {
        Self {
            ring: [0.0; LEN],
            pos: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.ring.iter_mut().for_each(|v| *v = 0.0);
        self.pos = 0;
    }
}

impl<const TAPS: usize> Qmf<TAPS> {
    pub(crate) fn new(proto: &[f64; TAPS], scale: f64) -> Self {
        let n = TAPS / 16;
        let mut coef = [0.0; TAPS];
        let mut k = 0;
        for i0 in (0..n / 2).step_by(GROUP) {
            for m in 0..16 {
                for i in i0..i0 + GROUP {
                    coef[k] = (proto[n * m + i] * scale) as f32;
                    coef[k + GROUP] = (proto[n * m + n - 1 - i] * scale) as f32;
                    k += 1;
                }
                k += GROUP;
            }
        }
        Self {
            coef,
            dct: Dct4::new(n),
        }
    }

    /// Synthesize one block: `input` holds `n` subband samples, `out` receives `n` PCM
    /// samples.
    #[inline]
    pub(crate) fn synth<const LEN: usize>(
        &self,
        st: &mut QmfState<LEN>,
        input: &[f32],
        out: &mut [f32],
    ) {
        match TAPS / 16 {
            32 => self.synth_n::<32, 16, LEN>(st, input, out),
            _ => self.synth_n::<64, 32, LEN>(st, input, out),
        }
    }

    #[inline]
    fn synth_n<const N: usize, const HALF: usize, const LEN: usize>(
        &self,
        st: &mut QmfState<LEN>,
        input: &[f32],
        out: &mut [f32],
    ) {
        debug_assert_eq!(LEN, 16 * N);
        // The new slot (time s) replaces the oldest one; slot (pos + m) & 15 holds s - m.
        st.pos = (st.pos + 15) & 15;
        let base = st.pos * N;
        let (u, w) = st.ring[base..base + N].split_at_mut(HALF);
        let u: &mut [f32; HALF] = u.try_into().expect("slot");
        let w: &mut [f32; HALF] = w.try_into().expect("slot");
        // V = DCT-IV(X), the 0.25 factor of C.3.6 folded into the prototype;
        // U[i] = V[i] - V[N-1-i] and W[i] = -(V[i] + V[N-1-i]) for i < N/2.
        self.dct.transform_pairs(input, |k, a, b| {
            if k < HALF {
                u[k] = a - b;
                w[k] = -(a + b);
            } else {
                u[N - 1 - k] = b - a;
                w[N - 1 - k] = -(b + a);
            }
        });
        // Slot m (time s - m) contributes U for even m and W for odd m. With
        // U[N-1-i] = -U[i] and W[N-1-i] = W[i], each loaded value serves output i and
        // its mirror N-1-i.
        let taps: [&[f32; HALF]; 16] = std::array::from_fn(|m| {
            let off = ((st.pos + m) & 15) * N + (m & 1) * HALF;
            st.ring[off..off + HALF].try_into().expect("slot")
        });
        let out: &mut [f32; N] = (&mut out[..N]).try_into().expect("block length");
        let coef = &self.coef[..16 * N];
        // Each group's accumulators are held over all 16 taps: on targets where f32
        // arithmetic stays scalar, they then live in registers.
        for (j, c) in coef.chunks_exact(32 * GROUP).enumerate() {
            let i0 = j * GROUP;
            let mut lo = [0f32; GROUP];
            let mut hi = [0f32; GROUP];
            for (c, g) in c.chunks_exact(4 * GROUP).zip(taps.chunks_exact(2)) {
                let (ce, co) = c.split_at(2 * GROUP);
                let ge = &g[0][i0..i0 + GROUP];
                let go = &g[1][i0..i0 + GROUP];
                for i in 0..GROUP {
                    lo[i] += ce[i] * ge[i];
                    hi[i] -= ce[GROUP + i] * ge[i];
                }
                for i in 0..GROUP {
                    lo[i] += co[i] * go[i];
                    hi[i] += co[GROUP + i] * go[i];
                }
            }
            for i in 0..GROUP {
                out[i0 + i] = lo[i];
                out[N - 1 - i0 - i] = hi[i];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Qmf, QmfState};

    /// Timing probe (development only): `cargo test --release -- --ignored qmf_speed --nocapture`.
    #[test]
    #[ignore = "timing probe; run on demand with --release"]
    fn qmf_speed() {
        fn probe<const TAPS: usize, const LEN: usize>(proto: &[f64; TAPS]) {
            let n = TAPS / 16;
            let q = Qmf::new(proto, 1.0);
            let mut st = QmfState::<LEN>::new();
            let mut x = vec![0.0f32; n];
            let mut out = vec![0.0f32; n];
            let iters = 2_000_000;
            let t = std::time::Instant::now();
            let mut acc = 0.0f32;
            for i in 0..iters {
                x.iter_mut()
                    .enumerate()
                    .for_each(|(k, v)| *v = ((i + k) % 7) as f32 - 3.0);
                q.synth(&mut st, &x, &mut out);
                acc += out[3];
            }
            let ns = t.elapsed().as_nanos() as f64 / iters as f64;
            eprintln!(
                "n={n}: {ns:.1} ns per block ({:.2} ns/sample) acc={acc}",
                ns / n as f64
            );
        }
        probe::<512, 512>(&crate::tables::QMF_NONPERFECT);
        probe::<1024, 1024>(&crate::tables::X96_FIR);
    }
}
