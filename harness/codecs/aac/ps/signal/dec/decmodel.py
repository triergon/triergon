"""Decorrelator of the reference's Parametric Stereo decoder, with its transient ducking, as
recovered by the dec probes (see harness/codecs/aac/ps/README.md for the probe behind
every value).

Signal domain: the decorrelator's own hybrid bands, H (n, nh) complex, aligned to zero delay
like the hyb recovery's hybrid slots. They are the hyb recovery's hybrid bands except that in the 20-band
configuration the QMF-0 sub-band pairs (0, 7) and (1, 6) are kept apart (the decorrelator
runs on each sub-band with its own frequency); `analysis(X, cfg)` forms them from QMF slots
with the hyb recovery's filters, and `to_hyb(D, cfg)` adds them back into the hyb recovery's layout.

Per band (frequency f in QMF band widths, parameter band b):
  * QMF bands k >= 35: D[t] = x[t - 1].
  * QMF bands 23..34: D[t] = x[t - 14].
  * all other bands (low hybrid bands and QMF bands NLOW..22): an all-pass chain
        D = phi z^-2 prod_m (-a_m + Q_m z^-d_m) / (1 - a_m Q_m z^-d_m),  d_m = 3, 4, 5,
    phi = exp(-i pi F0 f), Q_m = exp(-i pi QF_m f), a_m = A_m s, with
    s = 1 for the low hybrid bands and s = 1 - SLOPE (k - NLOW) for QMF band k
    (NLOW = 3 for 20 bands, 5 for 34 bands); f = k + 0.5 for a QMF band.
  * ducking: per parameter band, P[t] = sum over its bands of |H|^2;
        Pk = max(P, ALPHA Pk[t-1]); E1 += BETA (Pk - P - E1); E2 += BETA (P - E2);
        g = E2 / (GAMMA E1) if GAMMA E1 > E2 else 1;
    every D of the parameter band is multiplied by g[t]. All state starts at zero.
"""
import os
import sys

import numpy as np

# Constants and the probes that fit them (recover_dec.py refits them)
A = (0.65143909, 0.56471814, 0.48954165)  # D08/D10, refined by D25
QF = (0.43, 0.75, 0.347)                 # D08/D10, D25
F0 = 0.39                                # D08/D10, D25
SLOPE = 0.05                             # D08/D10, D25
DELAYS = (3, 4, 5)                     # D06
PRE = 2                                # D06: z^-2 before the sections
LONG, SHORT = 14, 1                    # D10/D11: plain delays
AP_END, SHORT_START = 23, 35           # D10/D12: first band of each plain-delay region
ALPHA, BETA, GAMMA = 0.76592832, 0.25, 1.5  # D21/D24, refined by D25
NLOW = {20: 3, 34: 5}

# Parameter band of each QMF band >= NLOW (the hyb recovery, h01/h03): (first QMF band, band)
_QB = {
    20: [(3, 8), (4, 9), (5, 10), (6, 11), (7, 12), (8, 13), (9, 14), (11, 15), (14, 16),
         (18, 17), (23, 18), (35, 19)],
    34: [(5, 16), (6, 17), (7, 18), (8, 19), (9, 20), (10, 21), (11, 22), (13, 23), (15, 24),
         (17, 25), (19, 26), (21, 27), (24, 28), (27, 29), (30, 30), (33, 31), (37, 32),
         (41, 33)],
}

# Low bands of the decorrelator: (QMF band k, split Q, sub-bands summed, parameter band, f).
# 20 bands: D15/D16 (and recover_dec stage 3). 34 bands: D26/D27 (and stage 3).
LOW = {
    20: [(0, 8, (0,), 0, 0.125), (0, 8, (7,), 0, -0.125), (0, 8, (1,), 1, 0.375),
         (0, 8, (6,), 1, -0.375), (0, 8, (2, 5), 2, 0.625), (0, 8, (3, 4), 3, 0.875),
         (1, 4, (1, 2), 4, 1.25), (1, 4, (0, 3), 5, 1.75), (2, 4, (0, 3), 6, 2.25),
         (2, 4, (1, 2), 7, 2.75)],
    34: None,  # filled in below
}


def sub_freq(k, Q, m):
    """Frequency (QMF band widths) the decorrelator uses for sub-band m of a Q-band split of
    QMF band k: k + (2m + 1)/Q - (k mod 2), moved by 2 into [k - 0.5, k + 1.5) (D16, D26,
    D27)."""
    f = k + (2 * m + 1) / Q - (k % 2)
    while f >= k + 1.5:
        f -= 2
    while f < k - 0.5:
        f += 2
    return f


# 34 bands: every sub-band apart (D26, D27), parameter band from the hyb recovery's map (h03).
_PAR34 = {(0, 0): 0, (0, 11): 0, (0, 1): 1, (0, 10): 1, (0, 2): 2, (0, 9): 2, (0, 3): 3,
          (0, 4): 4, (0, 5): 5, (0, 6): 6, (0, 7): 6, (0, 8): 7, (1, 2): 4, (1, 3): 5,
          (1, 4): 6, (1, 5): 7, (1, 6): 8, (1, 7): 9, (1, 0): 10, (1, 1): 10, (2, 3): 9,
          (2, 0): 10, (2, 1): 11, (2, 2): 12, (3, 1): 11, (3, 2): 12, (3, 3): 13, (3, 0): 14,
          (4, 3): 13, (4, 0): 14, (4, 1): 15, (4, 2): 16}
_Q34 = {0: 12, 1: 8, 2: 4, 3: 4, 4: 4}
LOW[34] = [(k, _Q34[k], (m,), _PAR34[(k, m)], sub_freq(k, _Q34[k], m))
           for k in range(5) for m in range(_Q34[k])]


def qmf_par(cfg, k):
    b = None
    for k0, bb in _QB[cfg]:
        if k >= k0:
            b = bb
    return b


def bands(cfg, low=None):
    """[(QMF band k, parameter band, f, kind)] in the decorrelator's band order; kind is
    'low' for a hybrid band (s = 1) or 'qmf'."""
    out = [(k, b, f, 'low') for k, Q, S, b, f in (low or LOW[cfg])]
    out += [(k, qmf_par(cfg, k), k + 0.5, 'qmf') for k in range(NLOW[cfg], 64)]
    return out


def _shift(x, d):
    o = np.zeros_like(x)
    o[d:] = x[:len(x) - d]
    return o


def _section(x, d, a, Q):
    """(-a + Q z^-d) / (1 - a Q z^-d) on x (zero state); x (n,) or (n, bands) with a, Q
    per band."""
    n = len(x)
    y = np.zeros(x.shape, complex)
    v = -a * x + Q * _shift(x, d)
    aq = a * Q
    for t0 in range(0, n, d):
        t1 = min(n, t0 + d)
        y[t0:t1] = v[t0:t1] + aq * y[t0 - d:t1 - d] if t0 >= d else v[t0:t1]
    return y


def allpass(x, f, s, consts=None):
    c = consts or {}
    Am = c.get('A', A)
    qf = c.get('QF', QF)
    f0 = c.get('F0', F0)
    y = np.exp(-1j * np.pi * f0 * f) * _shift(np.asarray(x, complex), c.get('PRE', PRE))
    for d, a, q in zip(DELAYS, Am, qf):
        y = _section(y, d, a * s, np.exp(-1j * np.pi * q * f))
    return y


def duck_gain(P, consts=None):
    """Ducking gain of power P (n,) or (n, groups); all state starts at zero."""
    c = consts or {}
    al, be, ga = c.get('ALPHA', ALPHA), c.get('BETA', BETA), c.get('GAMMA', GAMMA)
    P = np.asarray(P, float)
    g = np.ones(P.shape)
    pk = np.zeros(P.shape[1:])
    e1 = np.zeros(P.shape[1:])
    e2 = np.zeros(P.shape[1:])
    for t in range(len(P)):
        pk = np.maximum(P[t], al * pk)
        e1 = e1 + be * (pk - P[t] - e1)
        e2 = e2 + be * (P[t] - e2)
        duck = ga * e1 > e2
        g[t] = np.where(duck, e2 / np.where(duck, ga * e1, 1.0), 1.0)
    return g


class Decorrelator:
    """D of the decorrelator bands H (n, nh) of configuration cfg (20 or 34), zero state."""

    def __init__(self, cfg, consts=None, ducking=True):
        self.cfg = cfg
        self.c = consts or {}
        self.ducking = ducking
        self.bands = bands(cfg, self.c.get('LOW'))

    def band_linear(self, j, x):
        """Linear D of band j for its input x."""
        c = self.c
        k, b, f, kind = self.bands[j]
        f = c.get('FREQ', {}).get(j, f)
        if kind == 'qmf' and k >= c.get('SHORT_START', SHORT_START):
            return _shift(x, c.get('SHORT', SHORT))
        if kind == 'qmf' and k >= c.get('AP_END', AP_END):
            return _shift(x, c.get('LONG', LONG))
        k0 = c.get('K0', NLOW[self.cfg])
        s = 1.0 if kind == 'low' else 1 - c.get('SLOPE', SLOPE) * (k - k0)
        return allpass(x, f, s, c)

    def linear(self, H):
        """Linear D of all bands (the all-pass bands filtered together)."""
        c = self.c
        H = np.asarray(H, complex)
        D = np.zeros_like(H)
        ap, fs, ss = [], [], []
        k0 = c.get('K0', NLOW[self.cfg])
        for j, (k, b, f, kind) in enumerate(self.bands):
            f = c.get('FREQ', {}).get(j, f)
            if kind == 'qmf' and k >= c.get('SHORT_START', SHORT_START):
                D[:, j] = _shift(H[:, j], c.get('SHORT', SHORT))
            elif kind == 'qmf' and k >= c.get('AP_END', AP_END):
                D[:, j] = _shift(H[:, j], c.get('LONG', LONG))
            else:
                ap.append(j)
                fs.append(f)
                ss.append(1.0 if kind == 'low' else 1 - c.get('SLOPE', SLOPE) * (k - k0))
        if ap:
            fs, ss = np.array(fs), np.array(ss)
            y = np.exp(-1j * np.pi * c.get('F0', F0) * fs) * _shift(H[:, ap], c.get('PRE', PRE))
            for d, a, q in zip(DELAYS, c.get('A', A), c.get('QF', QF)):
                y = _section(y, d, a * ss, np.exp(-1j * np.pi * q * fs))
            D[:, ap] = y
        return D

    def gains(self, H):
        """(n, number of parameter bands) ducking gains."""
        H = np.asarray(H, complex)
        nb = 1 + max(b for _, b, _, _ in self.bands)
        P = np.zeros((len(H), nb))
        if self.c.get('DETECT') == 'band':  # variant for the probes: one gain per band
            return duck_gain(np.abs(H) ** 2, self.c)
        for j, (k, b, f, kind) in enumerate(self.bands):
            P[:, b] += np.abs(H[:, j]) ** 2
        return duck_gain(P, self.c)

    def process(self, H):
        D = self.linear(H)
        if self.ducking:
            G = self.gains(H)
            per_band = self.c.get('DETECT') == 'band'
            for j, (k, b, f, kind) in enumerate(self.bands):
                D[:, j] *= G[:, j if per_band else b]
        return D


# --- the hyb recovery's filters, for forming the decorrelator bands from QMF slots ------------

def _hyb():
    p = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb')
    if p not in sys.path:
        sys.path.insert(0, p)
    import hybmodel
    return hybmodel


def analysis(X, cfg, low=None):
    """Decorrelator bands (n, nh) from QMF slots X (n, 64) (X past the end is zero)."""
    hm = _hyb()
    X = np.asarray(X, complex)
    cols = [hm._fir(X[:, k], hm.filt(Q, S, cfg)) for k, Q, S, b, f in (low or LOW[cfg])]
    cols += [X[:, k] for k in range(NLOW[cfg], 64)]
    return np.stack(cols, 1)


def to_qmf(D, cfg, low=None):
    """QMF slots (n, 64): every decorrelator band added into its QMF band."""
    out = np.zeros((len(D), 64), complex)
    for j, (k, b, f, kind) in enumerate(bands(cfg, low)):
        out[:, k] += D[:, j]
    return out


def decorrelate(X, cfg, consts=None):
    """D in the decorrelator's bands for QMF slots X (n, 64), zero initial state."""
    return Decorrelator(cfg, consts).process(analysis(X, cfg))


def to_hyb(D, cfg):
    """D in the hyb recovery's hybrid-band layout (hybmodel.bands(cfg)): decorrelator bands with the
    same QMF band and parameter band are added."""
    hm = _hyb()
    hb = hm.bands(cfg)
    out = np.zeros((len(D), len(hb)), complex)
    for j, (k, b, f, kind) in enumerate(bands(cfg)):
        i = next(i for i, (kk, _, bb) in enumerate(hb) if kk == k and bb == b)
        out[:, i] += D[:, j]
    return out
