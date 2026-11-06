#!/usr/bin/env python3
"""One markdown table row: per-channel worst RMS / max |diff| against NUMERIC-RULES limits.
usage: cmp_f32.py name layout nch ours.f32 ref.f32 ref_cng.f32 ours_nodither.f32 ref_nodither.f32 ours.json
"""
import json
import sys

import numpy as np

RMS, MAX = 8.8e-6, 6.1035e-5
name, lay, nch = sys.argv[1], sys.argv[2], int(sys.argv[3])


def load(p):
    return np.fromfile(p, '<f4').astype(np.float64).reshape(-1, nch)


def stat(a, b):
    if len(a) != len(b):
        return 'LEN %d/%d' % (len(a), len(b)), False
    d = a - b
    rms = np.sqrt(np.mean(d ** 2, axis=0)).max()
    mx = np.abs(d).max()
    ok = rms <= RMS and mx <= MAX
    return '%s rms %.1e max %.1e' % ('pass' if ok else 'FAIL', rms, mx), ok


ours, ref, cng, ond, rnd = (load(p) for p in sys.argv[4:9])
info = json.loads(open(sys.argv[9]).read().strip().splitlines()[-1])
s1, _ = stat(ours, ref)
s2, _ = stat(cng, ref)
s3, _ = stat(ond, rnd)
print('| %s | %s / %s | %d / %d | %s | %s | %s |' % (
    name, lay, info['layout'], len(ref), info['samples_per_channel'], s1, s2, s3))
