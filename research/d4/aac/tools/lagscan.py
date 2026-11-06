"""Scan integer sample lags between candidate and reference interleaved f32le (dev aid).

usage: python3.14 -I lagscan.py CAND REF CH [MAXLAG] [START] [LEN]
Prints max|d| and RMS for each lag in [-MAXLAG, MAXLAG] over samples [START, START+LEN).
A positive lag means the candidate is late (cand[n + lag] ~ ref[n]).
"""
import sys
import numpy as np

c = np.fromfile(sys.argv[1], dtype='<f4')
r = np.fromfile(sys.argv[2], dtype='<f4')
ch = int(sys.argv[3])
ml = int(sys.argv[4]) if len(sys.argv) > 4 else 8
st = int(sys.argv[5]) if len(sys.argv) > 5 else 4096
ln = int(sys.argv[6]) if len(sys.argv) > 6 else 65536
c = c.reshape(-1, ch).astype(np.float64)
r = r.reshape(-1, ch).astype(np.float64)
for lag in range(-ml, ml + 1):
    a = c[st + lag:st + lag + ln]
    b = r[st:st + ln]
    n = min(len(a), len(b))
    d = a[:n] - b[:n]
    print('%4d max %.3e rms %.3e' % (lag, np.abs(d).max(), np.sqrt((d * d).mean())))
