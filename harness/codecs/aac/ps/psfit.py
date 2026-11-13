"""Least-squares fits of the reference's PS output in terms of this decoder's PS input.

The reference output of a channel is the 64-band QMF synthesis of a processed QMF matrix.
The synthesis is real-linear in the matrix, so a model that is linear in its unknowns
(per-band complex gains, filter taps) is fitted by building one synthesised column per
unknown (real and imaginary part of each) and solving in float64."""
import numpy as np

import psmodel


def band_signal(X, k, coef, delay=0):
    """Matrix with only band k nonzero: coef * X[n - delay, k]."""
    Y = np.zeros_like(X)
    if delay >= 0:
        Y[delay:, k] = coef * X[:len(X) - delay, k]
    else:
        Y[:delay, k] = coef * X[-delay:, k]
    return Y


def columns(X, terms):
    """terms: list of (k, delay) or (k, delay, sequence) -> synthesised real columns for the
    real and imaginary part of a complex coefficient per term. A term's optional
    sequence replaces X[:, k] (a hybrid-domain signal, for instance)."""
    cols = []
    for t in terms:
        k, d = t[0], t[1]
        seq = X[:, k] if len(t) == 2 else t[2]
        s = np.zeros(len(seq), complex)
        if d >= 0:
            s[d:] = seq[:len(seq) - d]
        else:
            s[:d] = seq[-d:]
        for c in (1.0, 1j):
            cols.append(psmodel.synth_band(c * s, k))
    return np.array(cols).T


def solve(A, y, rows):
    a, *_ = np.linalg.lstsq(A[rows], y[rows], rcond=None)
    r = y[rows] - A[rows] @ a
    return a[0::2] + 1j * a[1::2], float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max())
