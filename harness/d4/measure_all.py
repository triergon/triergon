#!/usr/bin/env python3
"""Measure exact integer taps of the reference scaler for 1-D geometries (research tooling).

Horizontal geometries give 14-bit taps (sum 16384), vertical ones 12-bit taps (sum 4096);
rows whose impulses saturate (edge taps above 1.0) are marked invalid (sum check).
usage: measure_all.py OUTDIR h:N:M v:N:M ...
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from measure_taps import measure


def exact(t, axis):
    if axis == 'v':
        return np.round(t / 4).astype(np.int64)
    q = np.zeros(t.shape, dtype=np.int64)
    for (x, j) in zip(*np.nonzero(np.abs(t) > 0.01)):
        amp = 127 if t[x, j] > 0 else -128
        o = int(round(t[x, j] * amp / 64)); c0 = int(round(t[x, j]))
        cands = [c for c in range(c0 - 3, c0 + 4) if ((((c * amp + 128 * 16384) >> 3) + 4) >> 3) - 32768 == o]
        q[x, j] = cands[0] if len(cands) == 1 else c0
    return q


if __name__ == '__main__':
    out = sys.argv[1]; os.makedirs(out, exist_ok=True)
    for spec in sys.argv[2:]:
        axis, n, m = spec.split(':'); n, m = int(n), int(m)
        path = os.path.join(out, f'{axis}-{n}-{m}.npy')
        if os.path.exists(path): continue
        q = exact(measure(n, m, axis), axis)
        np.save(path, q)
        one = 16384 if axis == 'h' else 4096
        print(spec, 'valid rows', int((q.sum(1) == one).sum()), '/', m, flush=True)
