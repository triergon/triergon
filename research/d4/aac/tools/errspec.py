"""Error spectrum (64 QMF-width bins) of candidate vs reference over a sample range (dev aid).

usage: python3.14 -I errspec.py CAND REF CH CHANNEL START LEN
Prints per bin (width fs/128, i.e. one 64-band QMF channel) the error and signal energy in dB.
"""
import sys
import numpy as np
c = np.fromfile(sys.argv[1], dtype='<f4').reshape(-1, int(sys.argv[3]))[:, int(sys.argv[4])]
r = np.fromfile(sys.argv[2], dtype='<f4').reshape(-1, int(sys.argv[3]))[:, int(sys.argv[4])]
st, ln = int(sys.argv[5]), int(sys.argv[6])
w = np.hanning(ln)
E = np.abs(np.fft.rfft((c[st:st + ln] - r[st:st + ln]).astype(np.float64) * w)) ** 2
R = np.abs(np.fft.rfft(r[st:st + ln].astype(np.float64) * w)) ** 2
b = np.arange(len(E)) * 64 // (len(E))
for k in range(64):
    e = E[b == k].sum(); s = R[b == k].sum()
    print('%2d err %6.1f dB  sig %6.1f dB' % (k, 10 * np.log10(e + 1e-30), 10 * np.log10(s + 1e-30)))
