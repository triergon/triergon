#!/usr/bin/env python3
"""Observe the scaled-path YUV->RGBA arithmetic on constant (Y, U, V) blocks (research tooling).

Every frame is a 512x512 picture of 16x16 constant blocks (chroma 8x8), scaled to 600x600 by the
reference scale filter (any non-identity size takes the general scaling path); the centre of each
block is free of filter mixing. Saves out[cs][range] = uint8 array [Y, U, V, 3] for the sampled grid.

usage: observe_tables.py OUT.npz [step]
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
import numpy as np
from rgba_probe import run

step = int(sys.argv[2]) if len(sys.argv) > 2 else 4
levels = np.arange(0, 256, step)
if levels[-1] != 255: levels = np.append(levels, 255)
L = len(levels)
combos = np.array(np.meshgrid(levels, levels, levels, indexing='ij')).reshape(3, -1).T
B, G = 16, 32  # block size, blocks per side
centre = lambda i: int((i * B + B / 2) * 600 / 512)
res = {}
for cs in (5, 1, 9):
    for rng in (1, 2):
        frames = []
        for f0 in range(0, len(combos), G * G):
            chunk = combos[f0:f0 + G * G]
            chunk = np.vstack([chunk, np.repeat(chunk[-1:], G * G - len(chunk), 0)])
            grid = chunk.reshape(G, G, 3)
            Y = np.kron(grid[..., 0], np.ones((B, B), int))
            U = np.kron(grid[..., 1], np.ones((B // 2, B // 2), int))
            V = np.kron(grid[..., 2], np.ones((B // 2, B // 2), int))
            frames.append((Y, U, V))
        out = run(frames, cs=cs, rng=rng, chain='scale=600:600,format=rgba', out_shape=(600, 600))
        idx = np.array([centre(i) for i in range(G)])
        vals = out[:, idx][:, :, idx][..., :3].reshape(-1, 3)[:len(combos)]
        res[f'{cs}-{rng}'] = vals.reshape(L, L, L, 3)
        print(cs, rng, flush=True)
np.savez_compressed(sys.argv[1], levels=levels, **res)
