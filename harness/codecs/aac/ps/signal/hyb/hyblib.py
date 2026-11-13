"""Lane helpers of the hybrid-filterbank probes (the hyb recovery).

The main tool is `onehot_fit`: a stream whose IID (or ICC) parameters are neutral except
in one parameter band b is decoded by the reference; each output channel minus the
synthesis of the PS input X is then exactly the synthesis of (g - 1) times the band-b part
of X, with g the known mixing gain of that channel. The band-b part is fitted as an FIR on
X_k (taps lo..hi, tap d multiplying X_k[t - d]) in each low QMF band k < nfir and as one
complex gain in every other QMF band.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from pslib import *  # noqa: E402,F401,F403
import pslib  # noqa: E402

SQ2 = np.sqrt(2.0)
IID_COARSE = [0, 2, 4, 7, 10, 14, 18, 25]
IID_FINE = [0, 2, 4, 6, 8, 10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50]
ICC_COS = [1, 0.9841240, 0.9594738, 0.8946843, 0.8269341, 0.7071068, 0.4533211, 0]


def iid_db(i, fine=False):
    g = IID_FINE if fine else IID_COARSE
    return g[i] if i >= 0 else -g[-i]


def gains(i, fine=False):
    """(cl, cr) of IID index i with ICC index 0 (Purnhagen eq. 5, confirmed by e14)."""
    c = 10 ** (iid_db(i, fine) / 20)
    return SQ2 * c / np.sqrt(1 + c * c), SQ2 / np.sqrt(1 + c * c)


def neutral_idx(nb):
    return [0] * nb


def stream(path, idx_or_payloads, nframes, seed, iid_mode=1, icc=None, icc_mode=None,
           bins=None, kind='iid'):
    if isinstance(idx_or_payloads, str):
        pl = idx_or_payloads
    elif kind == 'iid':
        pl = payload(iid=idx_or_payloads, iid_mode=iid_mode, icc=icc,
                     icc_mode=icc_mode if icc_mode is not None else 1)
    else:
        pl = payload(icc=idx_or_payloads, icc_mode=iid_mode)
    write(path, [pl] * nframes, seed=seed, bins=bins)
    y = decode(path)
    X, _ = dump(path)
    return y, X


def dev_fit(y, X, nfir, lo, hi, start=2 * 2048, gain_bands=None):
    """Fit y - synth(X) as an FIR (taps lo..hi) on X_k for k < nfir plus one complex gain
    per QMF band in gain_bands (default nfir..63). Returns (fir (nfir, ntaps), gains dict,
    rms, max)."""
    gb = list(range(nfir, 64)) if gain_bands is None else list(gain_bands)
    terms = []
    for k in range(nfir):
        terms += [(k, s) for s in taps(X[:, k], lo, hi)]
    terms += [(k, X[:, k]) for k in gb]
    a, rms, mx = pslib.fit(y, terms, known=X, start=start)
    nt = hi - lo + 1
    fir = a[:nfir * nt].reshape(nfir, nt)
    g = dict(zip(gb, a[nfir * nt:]))
    return fir, g, rms, mx
