#!/usr/bin/env python3
"""Fit the reference's odd-fitted-width output stage (research tooling).

Random 8-bit pictures through 'scale=W:H,format=rgba' with odd W; the separable passes are taken
from rgba_model (luma W x H, chroma ceil(w/2) x ceil(h/2) -> W x H, i.e. full width) and the
unrounded vertical sums A = sum(tap12 * intermediate15) are compared with each output channel.
usage: odd_width_fit.py [SRC_W SRC_H DST_W DST_H CS RANGE]
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run
from rgba_full_model import mat

a = sys.argv[1:]
sw, sh, dw, dh, cs, rng = (int(v) for v in a) if a else (64, 48, 101, 76, 1, 1)
gen = np.random.default_rng(31)
Y = gen.integers(0, 256, (sh, sw)); U = gen.integers(0, 256, (sh // 2, sw // 2)); V = gen.integers(0, 256, (sh // 2, sw // 2))
out = run([(Y, U, V)], cs=cs, rng=rng, chain=f'scale={dw}:{dh},format=rgba', out_shape=(dh, dw))[0].astype(np.int64)


def acc(P, ow, oh):
    H = np.minimum((P.astype(np.int64) @ mat(P.shape[1], ow, 16384).T) >> 7, 32767)
    return mat(P.shape[0], oh, 4096) @ H


Ay, Au, Av = acc(Y, dw, dh), acc(U, dw, dh), acc(V, dw, dh)
np.savez(f'{os.environ["SAVE"]}/odd-{dw}x{dh}-{cs}-{rng}.npz', Ay=Ay, Au=Au, Av=Av, out=out) if os.environ.get('SAVE') else None
for ch, cols in (('R', (Ay, Av)), ('G', (Ay, Au, Av)), ('B', (Ay, Au))):
    o = out[..., 'RGB'.index(ch)].ravel()
    X = np.vstack([c.ravel() / 2 ** 19 for c in cols] + [np.ones(o.size)]).T
    sel = (o > 2) & (o < 253)
    p, *_ = np.linalg.lstsq(X[sel], o[sel] + 0.5, rcond=None)
    r = o[sel] + 0.5 - X[sel] @ p
    print(ch, 'coef per 8-bit unit', np.round(p, 6).tolist(), 'resid', round(float(r.min()), 3), round(float(r.max()), 3))
