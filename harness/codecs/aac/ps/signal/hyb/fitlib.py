"""Structured least-squares fits of reference output (the hyb recovery).

A fit model is a list of real unknowns, each with one column: the 64-band synthesis of a
complex slot sequence placed in one QMF band, scaled per channel by a known factor. With
the one-hot IID streams the deviation y_c - synth(X) of channel c is (g_c - 1) times the
synthesis of the band-b part of X, so both channels are fitted jointly with the factors
(g_L - 1, g_R - 1).
"""
import numpy as np

import psmodel
from pslib import shift


def sym(s, d):
    """s[t - d] + s[t + d] (d > 0), or s (d = 0)."""
    return s if d == 0 else shift(s, d) + shift(s, -d)


def asym(s, d):
    return shift(s, d) - shift(s, -d)


def columns(terms, start, end):
    """terms: list of (band k, complex sequence). One real column per term."""
    return np.array([psmodel.synth_band(s, k)[start:end] for k, s in terms]).T


def joint_fit(ys, factors, X, terms, start=2 * 2048, end=None, extra=None):
    """Fit sum_c factor_c * A theta = y_c - synth(X) over all channels jointly. `extra`: a
    list of per-channel known deviations to subtract first. Returns (theta, rms, max, per
    channel rms)."""
    n = len(X)
    end = end or 64 * n - 640
    base = psmodel.synthesis(X)[start:end]
    A = columns(terms, start, end)
    rows, rhs = [], []
    for c, (y, f) in enumerate(zip(ys, factors)):
        r = np.array(y[:64 * n], float)[start:end] - base
        if extra is not None:
            r = r - extra[c][start:end]
        rows.append(f * A)
        rhs.append(r)
    AA = np.vstack(rows)
    bb = np.concatenate(rhs)
    th, *_ = np.linalg.lstsq(AA, bb, rcond=None)
    res = bb - AA @ th
    m = len(rhs[0])
    per = [float(np.sqrt(np.mean(res[i * m:(i + 1) * m] ** 2))) for i in range(len(ys))]
    return th, float(np.sqrt(np.mean(res ** 2))), float(np.abs(res).max()), per, AA, res


def stderr(AA, res):
    """Standard errors of the fitted unknowns, from the residual variance."""
    s2 = np.mean(res ** 2)
    cov = np.linalg.pinv(AA.T @ AA) * s2
    return np.sqrt(np.diag(cov))


def group_fit(y, X, groups, start=2 * 2048, end=None):
    """Fit channel y - synth(X) as sum_g (G_g - 1) * synth(part_g of X), one complex G_g per
    group. groups: list of lists of (k, seq) with seq the group's part of QMF band k (for
    example the hybrid-filtered X_k, or X_k itself). Returns (G, rms, max)."""
    n = len(X)
    end = end or 64 * n - 640
    r = np.array(y[:64 * n], float)[start:end] - psmodel.synthesis(X)[start:end]
    cols = []
    for parts in groups:
        for u in (1.0, 1j):
            c = np.zeros(end - start)
            for k, s in parts:
                c += psmodel.synth_band(u * s, k)[start:end]
            cols.append(c)
    A = np.array(cols).T
    th, *_ = np.linalg.lstsq(A, r, rcond=None)
    res = r - A @ th
    return 1 + th[0::2] + 1j * th[1::2], float(np.sqrt(np.mean(res ** 2))), float(np.abs(res).max())


def config_groups(X, cfg):
    """Per parameter band of configuration cfg (hybmodel), its parts of X for group_fit."""
    import hybmodel as hm
    g = {}
    for k, f, b in hm.bands(cfg):
        s = X[:, k] if f is None else hm._fir(X[:, k], f)
        g.setdefault(b, []).append((k, s))
    return [g[b] for b in sorted(g)]
