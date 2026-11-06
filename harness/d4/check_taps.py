#!/usr/bin/env python3
"""Compare measured taps (measure_all.py) with the rgba_model generator (research tooling).
usage: check_taps.py TAPDIR [eps ...]"""
import sys, os, glob; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from fractions import Fraction as Fr
from rgba_model import taps

def compare(path, **kw):
    axis, n, m = os.path.basename(path)[:-4].split('-'); n, m = int(n), int(m)
    one = 16384 if axis == 'h' else 4096
    q = np.load(path); bad = []
    for x, (s, c) in enumerate(taps(n, m, one=one, **kw)):
        if q[x].sum() != one: continue
        row = np.zeros(n, dtype=np.int64); row[s:s + len(c)] = c
        if (row != q[x]).any(): bad.append(x)
    return bad

if __name__ == '__main__':
    for eps in [Fr(e) for e in (sys.argv[2:] or ["1/3000"])]:
        tot = 0
        for path in sorted(glob.glob(os.path.join(sys.argv[1], '*.npy'))):
            bad = compare(path, cutoff=eps); tot += len(bad)
            print(f'  cutoff={float(eps):.3g}', os.path.basename(path), len(bad), bad[:8])
        print("cutoff", float(eps), "total bad rows", tot)
