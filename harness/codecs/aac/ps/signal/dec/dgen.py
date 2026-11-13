"""Streams with a chosen core signal (decorrelator recovery).

psgen.Gen codes random codebook-1 quads. GenQ codes a chosen integer spectrum per frame with
codebook 11 (pairs, sign bits, escapes up to 8191) in all 47 scalefactor bands, so a core
signal designed in the time domain (a steady tone, a step, a burst) can be sent: `mdct`
gives the long-window sine-window MDCT coefficients of each 2048-sample block (hop 1024) of
a 22.05 kHz signal, `quantise` the integers for a global gain (dequantised value
sign(q) |q|^(4/3) 2^((gg - 100) / 4)). The decoder's output is the signal delayed by its
own latency; the probes never rely on the absolute delay, since they fit against the PS
input dump X of this decoder."""
import numpy as np

import psgen

N = 2048
_W = np.sin(np.pi * (np.arange(N) + 0.5) / N)
_n0 = (N / 2 + 1) / 2
_C = np.cos(2 * np.pi / N * np.outer(np.arange(N) + _n0, np.arange(N // 2) + 0.5))  # (2048, 1024)


def mdct(s):
    """(F, 1024) coefficients of the blocks s[1024 m : 1024 m + 2048], m = 0..F-1."""
    F = (len(s) - N) // 1024 + 1
    return np.array([(_W * s[1024 * m:1024 * m + N]) @ _C for m in range(F)])


def quantise(C, gg):
    a = 2.0 ** ((gg - 100) / 4)
    q = np.sign(C) * np.round((np.abs(C) / a) ** 0.75)
    if np.abs(q).max() > 8191:
        raise ValueError('spectrum too large for gain %d: %d' % (gg, np.abs(q).max()))
    return q.astype(int)


def gain_for(C, peak=6000):
    """Smallest global gain whose largest integer is at most `peak`."""
    m = np.abs(C).max()
    return int(np.ceil(100 + 4 * np.log2(m / peak ** (4 / 3))))


class GenQ(psgen.Gen):
    def __init__(self, **kw):
        super().__init__(np.random.default_rng(0), **kw)
        self.sp11 = psgen.load_book('SPECTRUM_11')
        self.q = None

    def sce(self, w, silent=False):
        q = self.q
        w.put(0, 3); w.put(0, 4)             # SCE, tag 0
        w.put(self.gg, 8)                     # global_gain
        w.put(0, 1); w.put(0, 2); w.put(0, 1)  # reserved, ONLY_LONG, sine window
        nsfb = self.max_sfb
        w.put(nsfb, 6); w.put(0, 1)           # max_sfb, no prediction
        w.put(11, 4)                          # section: codebook 11 for every band
        left = nsfb
        while left >= 31:
            w.put(31, 5); left -= 31
        w.put(left, 5)
        for _ in range(nsfb):
            w.code(self.sf, 60)               # scalefactor delta 0
        w.put(0, 1); w.put(0, 1); w.put(0, 1)  # pulse, TNS, gain control
        for i in range(0, psgen.SWB_LONG_24[nsfb], 2):
            a, b = int(q[i]), int(q[i + 1])
            ua, ub = min(abs(a), 16), min(abs(b), 16)
            w.code(self.sp11, 17 * ua + ub)
            for v in (a, b):
                if v:
                    w.put(1 if v < 0 else 0, 1)
            for v in (a, b):
                if abs(v) >= 16:
                    e = abs(v)
                    nb = e.bit_length() - 1      # e = 2^nb + rest, nb >= 4
                    w.put((1 << (nb - 4 + 1)) - 2, nb - 4 + 1)  # nb-4 ones, then a zero
                    w.put(e - (1 << nb), nb)


def write_signal(path, s, gg=None, payloads=None, peak=6000, **kw):
    """Code the 22.05 kHz signal s (length 1024 (F + 1)) as F frames. payloads: one PS
    payload per frame (bit string, None for no PS, or a list of (id, bits) extensions)."""
    C = mdct(s)
    if gg is None:
        gg = gain_for(C, peak)
    Q = quantise(C, gg)
    g = GenQ(global_gain=gg, **kw)
    fr = []
    for m, q in enumerate(Q):
        g.q = q
        p = payloads[m] if payloads is not None else None
        if p is None:
            fr.append(g.frame([]))
        elif isinstance(p, list):
            fr.append(g.frame(p))
        else:
            fr.append(g.frame([(2, p)]))
    psgen.write_stream(path, fr)
    return gg, Q
