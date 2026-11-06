#!/usr/bin/env python3
"""List quantisation steps whose exact value lies near a rounding tie, with the reference's
decision (research tooling). usage: near_ties.py TAPDIR... (margin threshold 1e-3 tap units)"""
import sys, os, glob, math; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from fractions import Fraction as Fr
import rgba_model
events = []
for d in sys.argv[1:]:
    for path in sorted(glob.glob(os.path.join(d, '*.npy'))):
        axis, n, m = os.path.basename(path)[:-4].split('-'); n, m = int(n), int(m)
        one = 16384 if axis == 'h' else 4096
        q = np.load(path)
        T = rgba_model.taps(n, m, one=one)
        # re-run the diffusion to get margins: rebuild the folded, normalised rows
        wins = rgba_model.raw_windows(n, m)
        # apply the same trim as taps() by matching starts/lengths
        for x, (lo, coefs) in enumerate(T):
            if q[x].sum() != one: continue
            s, w = wins[x]
            # trimmed leading taps: drop until start matches the folded lo
            hi = lo + len(coefs)
            f = [Fr(0)] * len(coefs)
            # recompute folded weights from the trimmed window
            k0 = s
            while True:
                lo2 = max(k0, 0)
                if lo2 >= lo or k0 >= s + len(w) - 1: break
                k0 += 1
            ww = w[k0 - s:]
            for j, v in enumerate(ww):
                at = min(max(k0 + j, 0), n - 1)
                if lo <= at < hi: f[at - lo] += v
            tot = sum(f)
            if tot == 0: continue
            err = Fr(0); cum = Fr(0)
            for j, v in enumerate(f):
                val = v / tot * one + err
                r = math.floor(val + Fr(1, 2)); margin = val + Fr(1, 2) - math.floor(val + Fr(1, 2))  # in [0,1)
                dist = min(margin, 1 - margin)
                cum += v / tot * one
                if dist < Fr(1, 1000):
                    meas = int(q[x, lo + j])
                    events.append((float(dist), os.path.basename(path)[:-4], x, j, float(val), r, meas, float(cum), len(f)))
                err = val - r
events.sort()
for e in events[:60]: print(e)
print(len(events), 'events; mismatching decisions', sum(1 for e in events if e[5] != e[6]))
