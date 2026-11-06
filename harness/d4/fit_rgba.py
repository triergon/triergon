#!/usr/bin/env python3
"""Fit the identity-size YUV->RGBA conversion against native observations."""
import sys, itertools
import numpy as np
from pathlib import Path
obs = Path(sys.argv[1]); w, h = 1920, 1080
raw = np.frombuffer((obs / 'rand-1920x1080.yuv').read_bytes(), dtype=np.uint8)
Y = raw[:w*h].reshape(h, w).astype(np.int64)
U = raw[w*h:w*h + w*h//4].reshape(h//2, w//2).astype(np.int64)
V = raw[w*h + w*h//4:].reshape(h//2, w//2).astype(np.int64)
Un = np.repeat(np.repeat(U, 2, 0), 2, 1); Vn = np.repeat(np.repeat(V, 2, 0), 2, 1)
def predict(kr, kb, full, shift=16, yround=True):
    kg = 1 - kr - kb
    ys, cs, yo = (1.0, 1.0, 0) if full else (255/219, 255/224, 16)
    S = 1 << shift
    cy = round(ys*S); crv = round(2*(1-kr)*cs*S); cbu = round(2*(1-kb)*cs*S)
    cgu = round(2*(1-kb)*kb/kg*cs*S); cgv = round(2*(1-kr)*kr/kg*cs*S)
    l = (Y - yo) * cy; u = Un - 128; v = Vn - 128; half = S >> 1
    r = (l + crv*v + half) >> shift; g = (l - cgu*u - cgv*v + half) >> shift; b = (l + cbu*u + half) >> shift
    return np.stack([np.clip(r,0,255), np.clip(g,0,255), np.clip(b,0,255)], -1)
for space, rng, kr, kb, full in [('bt709','tv',.2126,.0722,False),('bt470bg','tv',.299,.114,False),('bt709','pc',.2126,.0722,True),('unknown','tv',.299,.114,False)]:
    o = np.frombuffer((obs / f'rand-1920x1080-{space}-{rng}.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4)[..., :3].astype(np.int64)
    p = predict(kr, kb, full)
    d = p - o
    print(space, rng, 'exact', round(float(np.mean(d == 0)), 4), 'max', int(np.abs(d).max()), 'per-channel mean', d.reshape(-1,3).mean(0).round(3))
    # chroma interpolation check: does output vary within a 2x2 block with constant Y?

print('--- variant search (bt709 tv)')
o = np.frombuffer((obs / 'rand-1920x1080-bt709-tv.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4)[..., :3].astype(np.int64)
of = np.frombuffer((obs / 'rand-1920x1080-bt709-pc.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4)[..., :3].astype(np.int64)
def variant(kr, kb, full, shift, offset_mode, cround, staged):
    kg = 1 - kr - kb
    ys, cs, yo = (1.0, 1.0, 0) if full else (255/219, 255/224, 16)
    S = 1 << shift
    q = (lambda x: int(round(x))) if cround == 'round' else (lambda x: int(np.floor(x)))
    cy = q(ys*S); crv = q(2*(1-kr)*cs*S); cbu = q(2*(1-kb)*cs*S); cgu = q(2*(1-kb)*kb/kg*cs*S); cgv = q(2*(1-kr)*kr/kg*cs*S)
    u = Un - 128; v = Vn - 128
    add = {'half': S >> 1, 'none': 0}[offset_mode]
    if staged:  # luma scaled and rounded to an intermediate first
        l = ((Y - yo) * cy + (S >> 1)) >> shift
        r = l + ((crv*v + add) >> shift); g = l - ((cgu*u + cgv*v - add) >> shift if False else ((cgu*u + cgv*v) >> shift)); b = l + ((cbu*u + add) >> shift)
    else:
        l = (Y - yo) * cy
        r = (l + crv*v + add) >> shift; g = (l - cgu*u - cgv*v + add) >> shift; b = (l + cbu*u + add) >> shift
    return np.stack([np.clip(r,0,255), np.clip(g,0,255), np.clip(b,0,255)], -1)
best = []
for shift, om, cr, st in itertools.product([8, 10, 12, 13, 14, 15, 16, 22], ['half', 'none'], ['round', 'floor'], [False, True]):
    p = variant(.2126, .0722, False, shift, om, cr, st); e = float(np.mean(p == o))
    pf = variant(.2126, .0722, True, shift, om, cr, st); ef = float(np.mean(pf == of))
    best.append((e + ef, e, ef, shift, om, cr, st))
for b in sorted(best, reverse=True)[:8]: print(b)
