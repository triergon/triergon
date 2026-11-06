#!/usr/bin/env python3
"""Reference fingerprints of FFmpeg's float output for the crate's offline parity tests.

usage: fingerprint.py NAME REF.f32 CHANNELS RATE [BLOCK=4096] [SPOTS=64] >> OUT.txt
Per stream: per-channel sums over BLOCK-sample blocks and SPOTS spot samples per channel, at
full float precision. The tests check spot samples against the per-sample bound
(6.1035e-5) and block sums against B * 8.8e-6 (Cauchy-Schwarz with the RMS bound).
"""
import sys
import numpy as np
name, path, ch, rate = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
B = int(sys.argv[5]) if len(sys.argv) > 5 else 4096
NS = int(sys.argv[6]) if len(sys.argv) > 6 else 64
x = np.fromfile(path, dtype='<f4').reshape(-1, ch).astype(np.float64)
n = x.shape[0]
print('stream %s %d %d %d' % (name, ch, n, rate))
for c in range(ch):
    sums = [x[i:i + B, c].sum() for i in range(0, n, B)]
    print('blocks %d %d %s' % (c, B, ' '.join('%.9e' % v for v in sums)))
    step = max(1, n // NS)
    pos = [(k * step + 17 * c + 5) % n for k in range(min(NS, n))]
    print('spots %d %s' % (c, ' '.join('%d:%.9e' % (p, x[p, c]) for p in pos)))
