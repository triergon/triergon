#!/usr/bin/env python3
"""Fit the float->s16 conversion of the observed device path (research tooling).
Uses the reported matrix (float32 arithmetic as the resampler does) and tests
round-to-nearest, floor and truncation against the CLI s16 output."""
import json, re, sys
import numpy as np
from pathlib import Path
obs = Path(sys.argv[1]); records = json.loads((obs / 'OBSERVATIONS.json').read_text())
order = {'FL': 0, 'FR': 1, 'FC': 2, 'LFE': 3, 'BL': 4, 'BR': 5, 'FLC': 6, 'FRC': 7, 'BC': 8, 'SL': 9, 'SR': 10}
result = []
for r in records:
    name = Path(r['vector']).name
    st = r['stream'].get('streams', [{}])[0]; ch = st.get('channels', 2)
    x = np.fromfile(obs / (name + '.f32'), dtype='<f4').reshape(-1, ch)
    y = np.fromfile(obs / (name + '.s16'), dtype='<i2').reshape(-1, 2).astype(np.int64)
    n = min(len(x), len(y)); x, y = x[:n], y[:n]
    if r['matrix']:
        cols = [re.findall(r'(\w+):(-?[0-9.]+)', row.split(': ', 1)[1]) for row in r['matrix']]
        names = [c for c, _ in cols[0]]
        m = np.array([[float(v) for _, v in row] for row in cols], dtype=np.float32).T  # in x out
        mixed = (x.astype(np.float32) @ m).astype(np.float32)
    else:
        mixed = x.astype(np.float32)
    scaled = (mixed * np.float32(32768)).astype(np.float64)
    fits = {k: float(np.mean(np.clip(f(scaled), -32768, 32767) != y)) for k, f in
            [('round', np.rint), ('floor', np.floor), ('trunc', np.trunc), ('floor+0.5', lambda v: np.floor(v + 0.5))]}
    best = min(fits, key=fits.get)
    p = np.clip({'round': np.rint, 'floor': np.floor, 'trunc': np.trunc, 'floor+0.5': lambda v: np.floor(v + 0.5)}[best](scaled), -32768, 32767)
    result.append(dict(vector=name, samples=int(n), mismatch_fraction=fits, best=best, max_abs_lsb=int(np.abs(p - y).max())))
    print(name, n, best, {k: round(v, 6) for k, v in fits.items()}, 'max', int(np.abs(p - y).max()))
(obs / 'ROUNDING.json').write_text(json.dumps(result, indent=1) + '\n')
