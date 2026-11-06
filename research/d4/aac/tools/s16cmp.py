"""Compare decoder f32 output with a conformance .s16 reference (independent of FFmpeg).

usage: python3.14 -I s16cmp.py CAND.f32 REF.s16 CH [MAXLAG]
Scales the candidate to 16-bit, searches the integer lag (|lag| <= MAXLAG, default 4096)
with the smallest RMS over the overlap, and prints max |d| / RMS in LSB16 at that lag.
"""
import sys
import numpy as np
ch = int(sys.argv[3])
c = np.fromfile(sys.argv[1], dtype='<f4').reshape(-1, ch).astype(np.float64) * 32768.0
r = np.fromfile(sys.argv[2], dtype='<i2')
r = r[:len(r) // ch * ch].reshape(-1, ch).astype(np.float64)
ml = int(sys.argv[4]) if len(sys.argv) > 4 else 4096
seg = slice(20000, 20000 + 65536)
best = None
for lag in range(-ml, ml + 1, 1):
    a = c[seg.start + lag: seg.stop + lag]
    b = r[seg]
    if len(a) != len(b) or seg.start + lag < 0:
        continue
    e = np.sqrt(((a - b) ** 2).mean())
    if best is None or e < best[0]:
        best = (e, lag)
lag = best[1]
n0 = max(0, -lag)
n = min(len(c) - lag, len(r)) - n0
a = np.clip(np.round(c[n0 + lag:n0 + lag + n]), -32768, 32767)
b = r[n0:n0 + n]
d = np.abs(a - b)
print('lag %d  samples %d (cand %d ref %d)  max %.0f LSB  rms %.3f LSB  >1LSB %.4f%%' % (
    lag, n, len(c), len(r), d.max(), np.sqrt((d ** 2).mean()), 100.0 * (d > 1).mean()))
