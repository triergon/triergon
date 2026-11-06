#!/usr/bin/env python3
"""Measure the reference scaler's horizontal and vertical luma/chroma taps (research tooling).

Uses single-sample impulses (+127/-128 on a 128 background) through the reference scale
filter with a 16-bit grey output, where one output code is 1/64 of a 14-bit tap unit, so
each tap is recovered to well under one unit. Horizontal taps come from rows (vertical size
unchanged, every row an independent experiment), vertical taps from columns.

usage: measure_taps.py SRC_LEN DST_LEN OUT.npy [h|v]
"""
import sys; sys.path.insert(0, __import__('os').path.dirname(__file__))
import numpy as np
from rgba_probe import run


def measure(n, m, axis="h", other=64, amps=(127, -128)):
    """Returns float matrix taps[m, n] (in 1/16384 units)."""
    taps = np.zeros((m, n))
    for amp in amps:
        for base in range(0, n, other):
            cnt = min(other, n - base)
            if axis == 'h':
                Y = np.full((other, n), 128)
                for r in range(cnt): Y[r, base + r] += amp
                C = np.full((other // 2, (n + 1) // 2), 128)
                o = run([(Y, C, C)], cs=5, rng=2, chain=f'scale={m}:{other},format=gray16le', out_shape=(other, m), out_bpp=2)[0]
                o = o.view('<u2')[..., 0].astype(float)
                for r in range(cnt):
                    t = (o[r] - 32768) * 64 / amp
                    sel = (o[r] > 0) & (o[r] < 65535)
                    taps[sel, base + r] = np.where(amp > 0, t, t)[sel] if amp > 0 else np.where(taps[sel, base + r] <= 0, t[sel], taps[sel, base + r])
            else:
                Y = np.full((n, other), 128)
                for r in range(cnt): Y[base + r, r] += amp
                C = np.full(((n + 1) // 2, other // 2), 128)
                o = run([(Y, C, C)], cs=5, rng=2, chain=f'scale={other}:{m},format=gray16le', out_shape=(m, other), out_bpp=2)[0]
                o = o.view('<u2')[..., 0].astype(float)
                for r in range(cnt):
                    t = (o[:, r] - 32768) * 64 / amp
                    if amp > 0:
                        taps[:, base + r] = t
                    else:
                        neg = t < 0
                        taps[neg, base + r] = t[neg]
    return taps


if __name__ == '__main__':
    n, m, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
    axis = sys.argv[4] if len(sys.argv) > 4 else 'h'
    amps = tuple(int(a) for a in sys.argv[5].split(",")) if len(sys.argv) > 5 else (127, -128)
    t = measure(n, m, axis, amps=amps)
    np.save(out, t)
    s = t.sum(1)
    print('rows', m, 'sum range', s.min(), s.max())
