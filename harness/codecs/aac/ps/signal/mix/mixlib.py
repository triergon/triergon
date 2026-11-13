"""Helpers of the mix recovery: windowed exact time-domain fits.

The 64-band synthesis is linear and slot-shift invariant (output slot t depends on input
slots t-10..t), so a fit over output slots [s0, s1) needs only input slots s0-10..s1-1.
`Window` precomputes the residual target y - synthesis(known) once for the whole stream and
then fits short windows with locally synthesised columns.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from pslib import *  # noqa: F401,F403,E402
import pslib  # noqa: E402

SQ2 = np.sqrt(2.0)


def col(seq, k, s0, s1):
    """Output samples of slots [s0, s1) of a single-band sequence seq in band k."""
    pr, pi = psmodel.band_response(k)
    out = np.zeros((s1 - s0, 64))
    for b in range(11):
        lo = s0 - b
        seg = seq[lo:s1 - b] if lo >= 0 else np.concatenate([np.zeros(-lo, complex), seq[:s1 - b]])
        out += np.outer(seg.real, pr[b]) + np.outer(seg.imag, pi[b])
    return out.reshape(-1)


class Target:
    """Residual of one output channel after the synthesis of `known` slots."""

    def __init__(self, y, known=None):
        n = len(y) // 64
        self.y = np.array(y[:64 * n], float)
        if known is not None:
            m = min(n, len(known))
            self.y = self.y[:64 * m] - psmodel.synthesis(known[:m])
        self.n = len(self.y) // 64

    def fit(self, terms, s0, s1):
        """terms: list of (band, sequence); returns (coefs, rms, max, signal rms)."""
        yy = self.y[64 * s0:64 * s1]
        cols = []
        for k, s in terms:
            c = col(s, k, s0, s1)
            cols.append(c)
            cols.append(col(1j * s, k, s0, s1))
        A = np.array(cols).T
        a, *_ = np.linalg.lstsq(A, yy, rcond=None)
        r = yy - A @ a
        return (a[0::2] + 1j * a[1::2], float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max()),
                float(np.sqrt(np.mean(yy ** 2))))

    def fit_real(self, cols_seq, s0, s1):
        """Fit with real coefficients on complex columns (band, sequence)."""
        yy = self.y[64 * s0:64 * s1]
        A = np.array([col(s, k, s0, s1) for k, s in cols_seq]).T
        a, *_ = np.linalg.lstsq(A, yy, rcond=None)
        r = yy - A @ a
        return a, float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max())


def zero_bands(X, bands):
    K = X.copy()
    K[:, list(bands)] = 0
    return K


def iid_c(db):
    c = 10 ** (db / 20.0)
    return SQ2 * c / np.sqrt(1 + c * c), SQ2 / np.sqrt(1 + c * c)


IID_COARSE = [0, 2, 4, 7, 10, 14, 18, 25]
IID_FINE = [0, 2, 4, 6, 8, 10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50]
ICC_VAL = [1, 0.937, 0.84118, 0.60092, 0.36764, 0, -0.589, -1]


def iid_db(i, fine):
    g = IID_FINE if fine else IID_COARSE
    return np.sign(i) * g[abs(i)]


def slot_gains(T, X, bands, u0, u1):
    """Per-slot complex gains g[t, j] with output = synthesis of g[t, j] X[t, bands[j]] in the
    bands, fitted on output slots [u0, u1) of the Target T (unknowns for input slots
    u0-10..u1-1). Returns (g for slots u0..u1-1 (n, len(bands)), rms, max)."""
    yy = T.y[64 * u0:64 * u1]
    cols = []
    for k in bands:
        pr, pi = psmodel.band_response(k)
        for t in range(u0 - 10, u1):
            for u in (1.0, 1j):
                v = u * X[t, k]
                o = np.zeros((u1 - u0 + 20, 64))
                o[t - u0 + 10:t - u0 + 21] = v.real * pr + v.imag * pi
                cols.append(o[10:10 + u1 - u0].reshape(-1))
    A = np.array(cols).T
    a, *_ = np.linalg.lstsq(A, yy, rcond=None)
    r = yy - A @ a
    g = (a[0::2] + 1j * a[1::2]).reshape(len(bands), u1 - u0 + 10)[:, 10:].T
    return g, float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max())


def hat(n, tau, width=32):
    t = np.arange(n)
    return np.maximum(0.0, 1.0 - np.abs(t - tau) / width)


def knot_fit(T, X, bands, s0, s1, knots, width=32):
    """Gains piecewise linear in time with knots at slots `knots` (spacing `width`), one
    complex value per band and knot, fitted on output slots [s0, s1). Returns
    (values (len(knots), len(bands)), rms, max, signal rms)."""
    n = len(X)
    terms = []
    for k in bands:
        for tau in knots:
            terms.append((k, X[:, k] * hat(n, tau, width)))
    a, rms, mx, sr = T.fit(terms, s0, s1)
    return a.reshape(len(bands), len(knots)).T, rms, mx, sr
