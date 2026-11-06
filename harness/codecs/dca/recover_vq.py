#!/usr/bin/env python3
"""Black-box recovery of the ETSI TS 102 114 Annex D.10 VQ codebooks.

The spec (V1.2.1 .. V1.6.1) defines D.10.1 (ADPCM coefficients, 4096 x 4) and D.10.2
(high-frequency subband VQ, 1024 x 32) as normative but states "Due to its extensive size,
this table is not included here". This script recovers both from the *observed output* of
the FFmpeg binary (development oracle) on synthetic streams written by
`examples/synth.rs` (`adpcm_probe`, `hfvq_probe`). No decoder source is read.

Method: per probe band, a calibration episode gives the band's synthesis impulse response g_b
(in units of one calibration subband sample). Each test episode's output is the linear
superposition sum_b sum_t x_b[t] g_b[n - 32 t]; x is solved by least squares, then
  ADPCM: x[0] = 1 and x[t] = sum_k c_k x[t-k] (C.3.3)  ->  c_1..c_4, entries = round(c * 2^13)
  HF VQ: x[t] = (SCALE * v[t] / 2^4) / (q * step * SCALE) -> v[t] = round(x[t] * q * step * 16)
Every recovered entry must lie within 0.1 of an integer (observed: ADPCM 0.035, HF VQ 3e-5) and reproduce the full solved
sequence (ADPCM recursion over all 8 samples), else the script fails.

usage: recover_vq.py PROBE.dts.meta PROBE.ref.f32 OUT.txt
"""
import sys
import numpy as np

meta = dict(l.split('=', 1) for l in open(sys.argv[1]).read().split() if '=' in l)
ref = np.fromfile(sys.argv[2], dtype='<f4').astype(np.float64).reshape(-1, 5)
fs = int(meta['frame_samples'])
episode = int(meta['episode'])
bands = [int(b) for b in meta['bands'].split(',')]
first_cal = int(meta['first_cal'])
first_test = int(meta['first_test'])
count = int(meta['count'])
ns = int(meta['samples'])
kind = meta['kind']
win = fs * episode
# coded channel order C L R SL SR -> FFmpeg 5.0(side) FL FR FC SL SR
out_index = [2, 0, 1, 3, 4]

def seg(frame):
    return ref[frame * fs: frame * fs + win]

basis = []
for k, b in enumerate(bands):
    s = seg(first_cal + k * episode)
    g = s[:, 0]
    for ch in range(5):
        assert np.allclose(s[:, ch], g, atol=1e-6), 'calibration differs per channel'
    basis.append(g)

cols = []
for k in range(len(bands)):
    for t in range(ns):
        c = np.zeros(win)
        c[32 * t:] = basis[k][:win - 32 * t]
        cols.append(c)
A = np.array(cols).T
pinv = np.linalg.pinv(A)

step12 = 79692 / 4194304.0  # Annex D.2.1 ABITS 12
q = int(meta['q'])
results = {}
worst = 0.0
bad = 0
overflow = []
SCALE7_100 = 398107  # Annex D.1.2 index 100
assert int(meta['scale7']) == 100
nf = (count + 5 * len(bands) - 1) // (5 * len(bands))
for e in range(nf):
    s = seg(first_test + e * episode)
    for ch in range(5):
        y = s[:, out_index[ch]]
        x = pinv @ y
        resid = np.abs(A @ x - y).max()
        assert resid < 1e-5, ('residual', e, ch, resid)
        x = x.reshape(len(bands), ns)
        for k in range(len(bands)):
            idx = e * 5 * len(bands) + ch * len(bands) + k
            if idx >= count:
                continue
            xs = x[k]
            if kind == 'adpcm':
                assert abs(xs[0] - 1) < 1e-4, ('x0', idx, xs[0])
                c = np.zeros(4)
                for t in range(1, 5):
                    c[t - 1] = xs[t] - sum(c[j] * xs[t - 1 - j] for j in range(t - 1))
                v = c * 8192
                iv = np.rint(v)
                worst = max(worst, np.abs(v - iv).max())
                # verify the recursion over all samples with the integer coefficients
                rec = np.zeros(ns)
                rec[0] = 1
                ci = iv / 8192
                for t in range(1, ns):
                    rec[t] = sum(ci[j] * rec[t - 1 - j] for j in range(4) if t - 1 - j >= 0)
                # FFmpeg departs from the linear recursion once a reconstructed subband
                # sample reaches 2^23 (24-bit overflow); compare only before that point.
                x0abs = q * step12 * SCALE7_100
                lim = ns
                for t in range(ns):
                    if abs(xs[t]) * x0abs >= 2 ** 23 - 16:
                        lim = t + 1
                        break
                if lim < ns:
                    overflow.append(idx)
                assert lim > 5, ('saturation within the recovery window', idx)
                if np.abs(rec[:lim] - xs[:lim]).max() > 1e-4 * max(1, np.abs(xs[:lim]).max()):
                    print('recursion mismatch', idx, np.round(xs, 5), v, 'diff', rec - xs); bad += 1
                results[idx] = [int(a) for a in iv]
            else:
                v = xs * q * step12 * 16
                iv = np.rint(v)
                worst = max(worst, np.abs(v - iv).max())
                assert iv.min() >= -128 and iv.max() <= 127
                results[idx] = [int(a) for a in iv]
assert sorted(results) == list(range(count)), len(results)
assert worst < 0.1 and bad == 0, ('not integral', worst, bad)
with open(sys.argv[3], 'w') as f:
    f.write('# %s codebook recovered by harness/codecs/dca/recover_vq.py; max |v - round(v)| = %.2e\n' % (kind, worst))
    for i in range(count):
        f.write(' '.join(map(str, results[i])) + '\n')
print(kind, 'entries', count, 'worst fractional deviation %.3e' % worst, 'episodes reaching 2^23:', overflow)
