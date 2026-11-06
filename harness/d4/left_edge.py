#!/usr/bin/env python3
"""Which window start reproduces each measured left-edge row (research tooling).
usage: left_edge.py TAPDIR..."""
import sys, os, glob, math; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from fractions import Fraction as Fr
from rgba_model import keys, diffuse
for d in sys.argv[1:]:
    for path in sorted(glob.glob(os.path.join(d, '*.npy'))):
        axis, n, m = os.path.basename(path)[:-4].split('-'); n, m = int(n), int(m)
        one = 16384 if axis == 'h' else 4096
        q = np.load(path); inc = ((n << 16) + m // 2) // m
        scale = max(Fr(n, m), Fr(1)); R = math.ceil(2 * scale)
        for x in range(m):
            c = (Fr(inc - 65536, 2) + x * inc) / 65536
            s0 = math.floor(c) - R + 1
            if s0 >= 0: break
            if q[x].sum() != one: continue
            ok = []
            for s in range(s0 - 1, s0 + 4):
                for end in range(s + 1, s0 + 2 * R + 1):
                    f = np.zeros(n, dtype=object); f[:] = Fr(0)
                    for k in range(s, end): f[min(max(k, 0), n - 1)] += keys((k - c) / scale)
                    tot = sum(f[:end + 1])
                    if tot == 0: continue
                    hi = min(end, n)
                    qq = diffuse([v / tot * one for v in f[:hi]])
                    if list(q[x, :hi]) == qq and not q[x, hi:].any(): ok.append((s - s0, end - (s0 + 2 * R)))
            print(os.path.basename(path), 'R', R, 'x', x, 'c', round(float(c), 4), 'c/scale', round(float(c / scale), 4), 'start-normal, end-normal:', sorted(set(ok))[:6])
