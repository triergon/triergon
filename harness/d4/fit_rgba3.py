#!/usr/bin/env python3
"""Third-stage fit on the exact R(Y,V) table: doubling-multiply-high structures."""
import sys, itertools
import numpy as np
from pathlib import Path
T = np.load(Path(sys.argv[1]) / 'R_table.npy')
Yg, Vg = np.meshgrid(np.arange(256), np.arange(256), indexing='ij')
kr = .2126; ys, cs = 255/219, 255/224
best = []
def dmulh(a, b): return (2 * a * b) >> 16
for s_in, Qc, r, yo_mode, c_mode in itertools.product(range(0, 8), range(10, 16), range(0, 5), ['pre', 'coef'], ['pre', 'post']):
    cy = int(round(ys * (1 << Qc))); crv = int(round(2 * (1 - kr) * cs * (1 << Qc)))
    if cy >= 1 << 15 or crv >= 1 << 15: continue
    if yo_mode == 'pre':
        yt = dmulh((Yg - 16) << s_in, cy)
    else:
        yt = dmulh(Yg << s_in, cy) - int(round(16 * ys * (1 << (s_in + Qc - 15))))
    vt = dmulh((Vg - 128) << s_in, crv)
    tot = yt + vt
    out = ((tot + (1 << (r - 1))) >> r) if r else tot
    # result scale must be 8-bit: scale = 2^(s_in + Qc + 1 - 16 - r)
    if s_in + Qc + 1 - 16 - r != 0: continue
    p = np.clip(out, 0, 255)
    best.append((float(np.mean(p == T)), s_in, Qc, r, yo_mode))
for b in sorted(best, reverse=True)[:8]: print(b)
