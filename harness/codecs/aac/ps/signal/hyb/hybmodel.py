"""Hybrid filterbank of the reference's Parametric Stereo decoder, as recovered by the
`hyb` probes (see harness/codecs/aac/ps/README.md for the probe behind each value).

Slots are complex QMF slots X (n, 64), aligned so that the reference's output equals the
64-band synthesis of the PS output slots at zero delay. A hybrid band is the part of one
low QMF band k that belongs to one parameter band: H[t] = sum_d f[d] X_k[t - d] for
d = -6..6 (it reads six slots ahead, which the caller's X must hold). Every other QMF band
is one hybrid band (H = X_k). Synthesis adds the hybrid bands of each QMF band.

Each filter is f[d] = g_Q[6 + d] * sum_{m in S} exp(j w_m d), w_m = (2m + 1) pi / Q, with a
13-tap real symmetric prototype g_Q and the set S of sub-bands of a Q-band split that the
parameter band takes in that QMF band.
"""
import numpy as np

D = np.arange(-6, 7)

# g_Q[n] for n = 6..12 (g[12 - n] = g[n]). Fitted values are recover_hyb.py's (absolute
# precision about 1.5e-9). g_Q[6] = 1/Q and, for Q = 4, g[10] = 0 are what perfect
# reconstruction needs (neutral PS output equals the input); the fits give them to 3e-9.
# 20 bands, QMF 0 (Q = 8): 3GPP TS 26.405 Table 1, g0; the fit matches it to 3.9e-9 except at
# n = 10 (and 2), which no 20-band output depends on (each parameter band takes a pair m,
# 7 - m, whose sum vanishes at d = +-4).
G8_20 = [0.125, 0.11793710567217, 0.09885108575264, 0.07266113929591, 0.04546865930473,
         0.02270420949825, 0.00746082949812]
# 20 bands, QMF 1 and 2 (Q = 4): fitted; they differ from TS 26.405 Table 1, g1,2, by up to
# 8.4e-3. n = 8 and 12 multiply zero in the pair sums (d = +-2, +-6): not observable, set to 0.
G4_20 = [0.25, 0.2163508546, 0.0, 0.0515702819, 0.0, -0.0134314047, 0.0]
# 34 bands (QMF 0: Q = 12, QMF 1: Q = 8, QMF 2..4: Q = 4): fitted.
G12_34 = [1 / 12, 0.0810034809, 0.0742831383, 0.0639983138, 0.0514490831, 0.0381281121,
          0.0408118003]
G8_34 = [0.125, 0.1222245234, 0.1030734356, 0.0841704412, 0.0541789107, 0.0375271612,
         0.0156567574]
G4_34 = [0.25, 0.2327985624, 0.1648630361, 0.0777872411, 0.0, -0.0487149819, -0.0590821133]

# Low hybrid bands: (QMF band k, Q, sub-bands S, parameter band).
LOW = {
    20: [(0, 8, (0, 7), 0), (0, 8, (1, 6), 1), (0, 8, (2, 5), 2), (0, 8, (3, 4), 3),
         (1, 4, (1, 2), 4), (1, 4, (0, 3), 5), (2, 4, (0, 3), 6), (2, 4, (1, 2), 7)],
    34: [(0, 12, (0, 11), 0), (0, 12, (1, 10), 1), (0, 12, (2, 9), 2), (0, 12, (3,), 3),
         (0, 12, (4,), 4), (0, 12, (5,), 5), (0, 12, (6, 7), 6), (0, 12, (8,), 7),
         (1, 8, (2,), 4), (1, 8, (3,), 5), (1, 8, (4,), 6), (1, 8, (5,), 7), (1, 8, (6,), 8),
         (1, 8, (7,), 9), (1, 8, (0, 1), 10),
         (2, 4, (3,), 9), (2, 4, (0,), 10), (2, 4, (1,), 11), (2, 4, (2,), 12),
         (3, 4, (1,), 11), (3, 4, (2,), 12), (3, 4, (3,), 13), (3, 4, (0,), 14),
         (4, 4, (3,), 13), (4, 4, (0,), 14), (4, 4, (1,), 15), (4, 4, (2,), 16)],
}
NLOW = {20: 3, 34: 5}  # QMF bands split by the hybrid analysis

# Parameter band of every QMF band above the split ones (h01, h03).
_QB = {
    20: [(3, 8), (4, 9), (5, 10), (6, 11), (7, 12), (8, 13), (9, 14), (11, 15), (14, 16),
         (18, 17), (23, 18), (35, 19)],
    34: [(5, 16), (6, 17), (7, 18), (8, 19), (9, 20), (10, 21), (11, 22), (13, 23), (15, 24),
         (17, 25), (19, 26), (21, 27), (24, 28), (27, 29), (30, 30), (33, 31), (37, 32),
         (41, 33)],
}


def qmf_par(cfg):
    """Parameter band of each QMF band >= NLOW[cfg]."""
    out = {}
    starts = _QB[cfg] + [(64, None)]
    for (k0, b), (k1, _) in zip(starts, starts[1:]):
        for k in range(k0, k1):
            out[k] = b
    return out


def proto(Q, cfg):
    g7 = {(8, 20): G8_20, (4, 20): G4_20, (12, 34): G12_34, (8, 34): G8_34, (4, 34): G4_34}[(Q, cfg)]
    g7 = np.asarray(g7, float)
    return np.concatenate([g7[::-1], g7[1:]])


def filt(Q, subs, cfg):
    """Taps f[d], d = -6..6, of the hybrid band taking sub-bands `subs` of a Q-band split."""
    e = sum(np.exp(1j * (2 * m + 1) * np.pi / Q * D) for m in subs)
    f = proto(Q, cfg) * e
    if all((Q - 1 - m) in subs for m in subs):
        f = f.real.astype(complex)  # a mirror pair: real (exactly)
    return f


def bands(cfg):
    """Hybrid bands in order: [(QMF band k, taps or None, parameter band)]."""
    out = [(k, filt(Q, S, cfg), b) for k, Q, S, b in LOW[cfg]]
    out += [(k, None, b) for k, b in sorted(qmf_par(cfg).items())]
    return out


PAR20 = [b for _, _, b in bands(20)]
PAR34 = [b for _, _, b in bands(34)]
NH = {20: len(PAR20), 34: len(PAR34)}  # 69 and 86


def _fir(x, f):
    n = len(x)
    y = np.zeros(n, complex)
    for d, c in zip(D, f):
        if c == 0:
            continue
        if d >= 0:
            y[d:] += c * x[:n - d]
        else:
            y[:d] += c * x[-d:]
    return y


def analysis(X, cfg):
    """Hybrid slots (n, NH[cfg]) of QMF slots X (n, 64), zero delay; X past the end is zero."""
    X = np.asarray(X, complex)
    return np.stack([X[:, k] if f is None else _fir(X[:, k], f) for k, f, _ in bands(cfg)], 1)


def synthesis(H, cfg):
    """QMF slots (n, 64) from hybrid slots H (n, NH[cfg])."""
    out = np.zeros((len(H), 64), complex)
    for j, (k, _, _) in enumerate(bands(cfg)):
        out[:, k] += H[:, j]
    return out


def par_of_qmf_filter(cfg):
    """{(QMF band, parameter band): taps} for the split QMF bands (for fits)."""
    return {(k, b): filt(Q, S, cfg) for k, Q, S, b in LOW[cfg]}


PAR = {20: PAR20, 34: PAR34}


def config(iid_mode, icc_mode):
    """Configuration run for the enabled IID / ICC modes (None = disabled) (h05): 34 bands
    when either enabled mode is 2 or 5, else 20 bands."""
    return 34 if any(m is not None and m % 3 == 2 for m in (iid_mode, icc_mode)) else 20


# Coded band -> configured band (h05, h06). The same maps hold for IID (coarse and fine)
# and ICC indices, and for both ICC mixing procedures.
MAP10_20 = [b // 2 for b in range(20)]
# 20 -> 34: an int copies that band; a pair (a, b) is int((c[a] + c[b]) / 2), the index mean
# truncated toward zero.
MAP20_34 = [0, (0, 1), 1, 2, (2, 3), 3, 4, 4, 5, 5, 6, 7, 8, 8, 9, 9, 10, 11, 12, 13, 14, 14,
            15, 15, 16, 16, 17, 17, 18, 18, 18, 18, 19, 19]


def _mean0(a, b):
    s = a + b
    return s // 2 if s >= 0 else -((-s) // 2)


def to_config(idx, nb, cfg, kind='iid'):
    """Indices idx coded at nb bands, as applied in configuration cfg. `kind` (iid or icc)
    does not change the rule. 10 -> 34 is 10 -> 20 followed by 20 -> 34 (h05 checks it)."""
    idx = [int(v) for v in idx]
    if nb == cfg:
        return idx
    if nb == 10:
        idx = [idx[b] for b in MAP10_20]
        if cfg == 20:
            return idx
    if cfg == 34:
        return [idx[m] if isinstance(m, int) else _mean0(idx[m[0]], idx[m[1]]) for m in MAP20_34]
    raise ValueError('no mapping from %d bands to the %d-band configuration' % (nb, cfg))
