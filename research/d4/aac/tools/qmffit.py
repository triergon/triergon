"""Fit the SBR QMF bank conventions against a black-box decoder's pure-QMF output.

usage: python3.14 -I qmffit.py CORE.f32 CORE_CH MUTE_REF.f32 CH WINDOW.npy [START_SLOT] [SLOTS]

CORE.f32   core-rate output of the stream with its SBR elements removed (examples/strip_sbr.rs,
           decoded by this crate; its core output matches FFmpeg to ~1e-7), channel 0 used.
MUTE_REF   FFmpeg's 2x-rate output of the same stream with every SBR payload zeroed
           (examples/sbr_mute.rs): SBR is signalled but never starts, so the output is the
           32-band analysis -> 64-band synthesis of the core signal (6-slot low band delay).
WINDOW.npy the 640 window values in the order printed by ETSI TS 101 980 V1.1.1 Table I.1
           (np.save of crate table sbr::tables::QMF_WINDOW).

Candidates: analysis window offset (0..3) and orientation (as printed / reversed), synthesis
window offset and orientation, analysis conjugation, analysis phase A in (2n + A)/64,
synthesis phase S in (2n + S)/128; each run in float64 and compared after the best integer
delay. Result on CT_DecoderCheck sbr_i-ps_i (2026-11-03): reversed window c[n] = c_DRM[639 - n]
for both banks, A = -1/2, S = -255, delay 384: max |d| 9.5e-8; the next family is 1.0e-4.
"""
import itertools
import sys
import numpy as np

core = np.fromfile(sys.argv[1], dtype='<f4').reshape(-1, int(sys.argv[2]))[:, 0].astype(np.float64)
ref = np.fromfile(sys.argv[3], dtype='<f4').reshape(-1, int(sys.argv[4]))[:, 0].astype(np.float64)
drm = np.load(sys.argv[5])
s0 = int(sys.argv[6]) if len(sys.argv) > 6 else 1000
ns = int(sys.argv[7]) if len(sys.argv) > 7 else 300
z = np.concatenate([[0, 0], drm, [0, 0]])
x = core[32 * s0:32 * (s0 + ns)]
pad = np.concatenate([np.zeros(320), x])
segs = np.array([pad[32 * t + 32:32 * t + 352][::-1] for t in range(ns)])  # newest first


def run(wa, A, ca, ws, S):
    n = np.arange(64)
    k = np.arange(32)
    M = 2 * np.exp(ca * 1j * np.pi * np.outer(k + 0.5, 2 * n + A) / 64)
    kk = np.arange(64)
    nn = np.arange(128)
    N = np.exp(1j * np.pi * np.outer(kk + 0.5, 2 * nn + S) / 128) / 64
    W = (segs * wa).reshape(ns, 5, 64).sum(1) @ M.T
    V = np.real(np.concatenate([W, np.zeros((ns, 32))], 1) @ N)
    vb = np.zeros((ns + 10, 128))
    vb[10:] = V
    out = np.empty((ns, 64))
    for t in range(ns):
        g = np.empty(640)
        for i in range(5):
            g[128 * i:128 * i + 64] = vb[10 + t - 2 * i][0:64]
            g[128 * i + 64:128 * i + 128] = vb[10 + t - 2 * i - 1][64:128]
        out[t] = (g * ws).reshape(10, 64).sum(0)
    y = out.reshape(-1)[64 * 20:]
    best = (9.0, 0)
    for D in range(300, 480):
        r = ref[2 * 32 * s0 + 64 * 20 + D:2 * 32 * s0 + 64 * 20 + D + len(y)]
        e = np.abs(y - r).max()
        if e < best[0]:
            best = (e, D)
    return best


res = []
for ao, rev_a, so, rev_s, ca, A, S in itertools.product(range(4), (False, True), range(4), (False, True), (1, -1),
                                                         (-0.5, 0.5), (-257, -255, -253)):
    wa = z[ao:ao + 640][0::2]
    ws = z[so:so + 640]
    e, D = run(wa[::-1] if rev_a else wa, A, ca, ws[::-1] if rev_s else ws, S)
    res.append((e, ao, rev_a, so, rev_s, ca, A, S, D))
res.sort()
for r in res[:8]:
    print('max %.3e  win_a off %d rev %d  win_s off %d rev %d  conj %+d  A %+.1f  S %d  delay %d' % r)
