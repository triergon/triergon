#!/usr/bin/env python3
"""Final fit: high-precision luma, chroma terms floored to half output units, one rounding."""
import sys, itertools
import numpy as np
from pathlib import Path
obs = Path(sys.argv[1]); w, h = 1920, 1080
raw = np.frombuffer((obs / 'rand-1920x1080.yuv').read_bytes(), dtype=np.uint8)
Y = raw[:w*h].reshape(h, w).astype(np.int64)
U = np.repeat(np.repeat(raw[w*h:w*h + w*h//4].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
V = np.repeat(np.repeat(raw[w*h + w*h//4:].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
S = 15; F = S - 1
def q(term): return (term >> F) << F
for space, rng, kr, kb, full in [('bt709', 'tv', .2126, .0722, False), ('bt470bg', 'tv', .299, .114, False),
                                 ('bt709', 'pc', .2126, .0722, True), ('unknown', 'tv', .299, .114, False)]:
    o = np.frombuffer((obs / f'rand-1920x1080-{space}-{rng}.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4).astype(np.int64)
    kg = 1 - kr - kb
    ys, cs, yo = (1.0, 1.0, 0) if full else (255/219, 255/224, 16)
    c = lambda x: int(round(x * (1 << S)))
    cy, crv, cbu, cgu, cgv = c(ys), c(2*(1-kr)*cs), c(2*(1-kb)*cs), c(2*(1-kb)*kb/kg*cs), c(2*(1-kr)*kr/kg*cs)
    L = (Y - yo) * cy; u = U - 128; v = V - 128; half = 1 << (S - 1)
    R = np.clip((L + q(v * crv) + half) >> S, 0, 255)
    B = np.clip((L + q(u * cbu) + half) >> S, 0, 255)
    results = {}
    for name, g in [('sum-floored', L - q(u * cgu + v * cgv)), ('each-floored', L - q(u * cgu) - q(v * cgv)),
                    ('neg-each', L + q(-u * cgu) + q(-v * cgv)), ('neg-sum', L + q(-(u * cgu + v * cgv)))]:
        G = np.clip((g + half) >> S, 0, 255)
        results[name] = round(float(np.mean(G == o[..., 1])), 6)
    print(space, rng, 'R', round(float(np.mean(R == o[..., 0])), 6), 'B', round(float(np.mean(B == o[..., 2])), 6), 'G', results,
          'A', bool(np.all(o[..., 3] == 255)), (cy, crv, cbu, cgu, cgv))
