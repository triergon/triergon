"""Numerical model pieces for the PS table fits: this repository's 64-band QMF synthesis
(crates/codec-aac/src/sbr/qmf.rs, written out in float64) and the PS input dump of
crates/codec-aac/examples/ps_input.rs."""
import os
import subprocess

import numpy as np

import psana

ROOT = psana.ROOT
PS_INPUT = os.environ.get('PS_INPUT', os.path.join(ROOT, 'target', 'release', 'examples', 'ps_input'))


def ps_input(path):
    """(frames, 38, 64) complex: the QMF slots this decoder hands to PS per frame."""
    out = path + '.x.f32'
    subprocess.run([PS_INPUT, path, out], check=True)
    a = np.fromfile(out, dtype='<f4').astype(np.float64)
    a = a[0::2] + 1j * a[1::2]
    return a.reshape(-1, 38, 64)


def slots(x):
    """The current-frame rows of every frame as one (frames*32, 64) sequence."""
    return x[:, :32, :].reshape(-1, 64)


_SYN = None


def synthesis(X, scale=1.0 / 32768):
    """64-band synthesis of slots X (n, 64) into n*64 real samples."""
    global _SYN
    if _SYN is None:
        c = psana.qmf_window()  # c[n] = c_DRM[639 - n]
        k = np.arange(64)
        n = np.arange(128)
        N = np.exp(1j * np.pi * np.outer(n * 2 - 255, k + 0.5) / 128) / 64  # (128, 64)
        _SYN = (c, N)
    c, N = _SYN
    v = np.zeros(1280)
    out = np.empty(len(X) * 64)
    for t, row in enumerate(X):
        v[128:] = v[:-128]
        v[:128] = (N @ (row * scale)).real
        g = np.empty(640)
        for i in range(5):
            g[128 * i:128 * i + 64] = v[256 * i:256 * i + 64]
            g[128 * i + 64:128 * i + 128] = v[256 * i + 192:256 * i + 256]
        out[64 * t:64 * t + 64] = (g * c).reshape(10, 64).sum(0)
    return out


_IMP = {}


def band_response(k, scale=1.0 / 32768):
    """Output samples (11 slots) of a unit value (1 and i) in band k at slot 0."""
    if k not in _IMP:
        res = []
        for u in (1.0, 1j):
            X = np.zeros((11, 64), complex)
            X[0, k] = u
            res.append(synthesis(X, scale).reshape(11, 64))
        _IMP[k] = res
    return _IMP[k]


def synth_band(seq, k):
    """Synthesis of a matrix whose only nonzero band is k, holding the slot sequence seq."""
    pr, pi = band_response(k)
    n = len(seq)
    out = np.zeros((n + 11, 64))
    for b in range(11):
        out[b:b + n] += np.outer(seq.real, pr[b]) + np.outer(seq.imag, pi[b])
    return out[:n].reshape(-1)
