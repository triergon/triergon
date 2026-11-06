#!/usr/bin/env python3
"""Compare interleaved f32le candidate vs reference per the codec numeric rules.
usage: compare.py REF.f32 CAND.f32 CHANNELS [--json]"""
import sys, json
import numpy as np
ref = np.fromfile(sys.argv[1], dtype='<f4')
cand = np.fromfile(sys.argv[2], dtype='<f4')
ch = int(sys.argv[3])
res = {'ref_samples': int(ref.size // ch), 'cand_samples': int(cand.size // ch)}
ok = ref.size == cand.size and ref.size % ch == 0
n = min(ref.size, cand.size) // ch
r = ref[:n * ch].reshape(n, ch).astype(np.float64)
c = cand[:n * ch].reshape(n, ch).astype(np.float64)
d = c - r
rms = np.sqrt((d ** 2).mean(axis=0)) if n else np.zeros(ch)
mx = np.abs(d).max(axis=0) if n else np.zeros(ch)
res['rms'] = [float('%.3e' % v) for v in rms]
res['max'] = [float('%.3e' % v) for v in mx]
bad = np.where(np.abs(d).max(axis=1) > 6.1035e-05)[0]
res['first_bad_sample'] = int(bad[0]) if bad.size else None
ok = ok and bool((rms <= 8.8e-6).all() and (mx <= 6.1035e-5).all())
res['pass'] = ok
print(json.dumps(res))
