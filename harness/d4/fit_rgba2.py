#!/usr/bin/env python3
"""Second-stage fit: per-term truncating fixed-point products (vector-unit style)."""
import sys, itertools
import numpy as np
from pathlib import Path
obs = Path(sys.argv[1]); w, h = 1920, 1080
raw = np.frombuffer((obs / 'rand-1920x1080.yuv').read_bytes(), dtype=np.uint8)
Y = raw[:w*h].reshape(h, w).astype(np.int64)
U = np.repeat(np.repeat(raw[w*h:w*h + w*h//4].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
V = np.repeat(np.repeat(raw[w*h + w*h//4:].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
o = np.frombuffer((obs / 'rand-1920x1080-bt709-tv.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4).astype(np.int64)
kr, kb = .2126, .0722; kg = 1 - kr - kb
ys, cs = 255/219, 255/224
res = []
for p, Q, yoff, final_round in itertools.product([5, 6, 7, 8], [12, 13, 14, 15, 16], ['sub', 'post'], ['round', 'trunc']):
    cq = lambda x: int(round(x * (1 << Q)))
    cy, crv, cbu, cgu, cgv = cq(ys), cq(2*(1-kr)*cs), cq(2*(1-kb)*cs), cq(2*(1-kb)*kb/kg*cs), cq(2*(1-kr)*kr/kg*cs)
    if yoff == 'sub':
        yt = (((Y - 16) << p) * cy) >> Q
    else:
        yt = ((Y << p) * cy >> Q) - ((16 << p) * cy >> Q)
    vt = ((V - 128) << p) * crv >> Q
    ut = ((U - 128) << p) * cbu >> Q
    gt = (((U - 128) << p) * cgu >> Q) + (((V - 128) << p) * cgv >> Q)
    rnd = (1 << (p - 1)) if final_round == 'round' else 0
    R = np.clip((yt + vt + rnd) >> p, 0, 255); B = np.clip((yt + ut + rnd) >> p, 0, 255); G = np.clip((yt - gt + rnd) >> p, 0, 255)
    er, eg, eb = float(np.mean(R == o[..., 0])), float(np.mean(G == o[..., 1])), float(np.mean(B == o[..., 2]))
    res.append((er + eg + eb, round(er, 4), round(eg, 4), round(eb, 4), p, Q, yoff, final_round))
for r in sorted(res, reverse=True)[:6]: print(r)
