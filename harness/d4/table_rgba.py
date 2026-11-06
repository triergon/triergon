#!/usr/bin/env python3
"""Is each output channel a pure function of its inputs? Tabulate and test structure."""
import sys
import numpy as np
from pathlib import Path
obs = Path(sys.argv[1]); w, h = 1920, 1080
raw = np.frombuffer((obs / 'rand-1920x1080.yuv').read_bytes(), dtype=np.uint8)
Y = raw[:w*h].reshape(h, w).astype(np.int64)
U = np.repeat(np.repeat(raw[w*h:w*h + w*h//4].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
V = np.repeat(np.repeat(raw[w*h + w*h//4:].reshape(h//2, w//2).astype(np.int64), 2, 0), 2, 1)
o = np.frombuffer((obs / 'rand-1920x1080-bt709-tv.rgba').read_bytes(), dtype=np.uint8).reshape(h, w, 4).astype(np.int64)
key = Y * 256 + V
R = o[..., 0]
mn = np.full(65536, 999); mx = np.full(65536, -1)
np.minimum.at(mn, key.ravel(), R.ravel()); np.maximum.at(mx, key.ravel(), R.ravel())
seen = mx >= 0
print('R(Y,V) pairs seen', int(seen.sum()), 'conflicting', int(np.sum(mx[seen] != mn[seen])))
# separability: R = clip(a[Y] + b[V]) ? check unclipped region
T = np.where(seen, mn, -1).reshape(256, 256)
# estimate a[Y]=T[Y,128] and b[V]=T[100,V]-T[100,128]
a = T[:, 128]; b = T[100, :] - T[100, 128]
pred = a[:, None] + b[None, :]
mask = (T > 2) & (T < 253) & (pred > 2) & (pred < 253)
print('separable fit exact', float(np.mean((pred == T)[mask])), 'diff hist', np.unique((pred - T)[mask], return_counts=True))
np.save(obs / 'R_table.npy', T)
# luma alone: grey pixels U=V=128
grey = (U == 128) & (V == 128)
print('grey samples', int(grey.sum()))
