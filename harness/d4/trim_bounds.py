#!/usr/bin/env python3
"""Bound the leading-tap trim cutoff per geometry from measured taps (research tooling).
For each measured row, compare the quantised filter with 0..3 leading taps trimmed against the
measurement; the cumulative |w| / sum(w) of the trimmed taps brackets the cutoff.
usage: trim_bounds.py TAPDIR"""
import sys, os, glob, math; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from fractions import Fraction as Fr
from rgba_model import raw_windows, diffuse
for path in sorted(glob.glob(os.path.join(sys.argv[1], '*.npy'))):
    axis, n, m = os.path.basename(path)[:-4].split('-'); n, m = int(n), int(m)
    one = 16384 if axis == 'h' else 4096
    q = np.load(path); W = raw_windows(n, m)
    lo, hi = Fr(0), Fr(1)
    for x in range(m):
        s, w = W[x]
        if q[x].sum() != one or s < 0 or s + len(w) > n: continue
        tot = sum(w); outs = []
        for k in range(0, min(4, len(w) - 1)):
            t = sum(w[k:]); cum = sum(abs(v) for v in w[:k])
            outs.append((cum / tot, [0] * k + diffuse([v / t * one for v in w[k:]])))
        meas = q[x, s:s + len(w)].tolist()
        ok = [i for i, (_, o) in enumerate(outs) if o == meas]
        if not ok: print('  neither', x); continue
        k = ok[0] if len(ok) == 1 else None
        if k is None: continue
        lo = max(lo, outs[k][0])
        if k + 1 < len(outs) and outs[k + 1][1] != meas: hi = min(hi, outs[k + 1][0])
    s = max(Fr(n, m), 1)
    print(os.path.basename(path), 'scale', round(float(s), 4), 'cutoff in', float(lo), float(hi), ' x6s:', float(lo * 6 * s), float(hi * 6 * s))
