"""Helpers of the decorrelator recovery (dec). Everything here is built from pslib and the
established facts of the brief (20-band QMF map of TS 26.405 Table 2 for k >= 3; ICC
index 7 with IID 0 gives L = D, R = -D; ICC index 0 gives L = R = M)."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from pslib import *  # noqa: F401,F403,E402
import pslib  # noqa: E402

# 20-band parameter band of QMF band k >= 3 (TS 26.405 Table 2; established by e14)
_EDGES20 = [(3, 3, 8), (4, 4, 9), (5, 5, 10), (6, 6, 11), (7, 7, 12), (8, 8, 13),
            (9, 10, 14), (11, 13, 15), (14, 17, 16), (18, 22, 17), (23, 34, 18), (35, 63, 19)]


def par20(k):
    for lo, hi, b in _EDGES20:
        if lo <= k <= hi:
            return b
    raise ValueError(k)


def qmf_of_par20(b):
    for lo, hi, bb in _EDGES20:
        if bb == b:
            return list(range(lo, hi + 1))
    raise ValueError(b)


def icc_payload(dec_bands, nb=20, mode=None, header=True, nenv=1):
    """ICC only: index 7 in the parameter bands `dec_bands`, index 0 elsewhere."""
    if mode is None:
        mode = {10: 0, 20: 1, 34: 2}[nb]
    idx = [7 if b in dec_bands else 0 for b in range(nb)]
    return pslib.payload(icc=idx, icc_mode=mode, header=header, nenv=nenv)


def run(path, payloads, seed=7, bins=None, **kw):
    """Write, decode and dump a stream: (y (n, 2), X (n, 64))."""
    pslib.write(path, payloads, seed=seed, bins=bins, **kw)
    y = pslib.decode(path)
    X, _ = pslib.dump(path)
    return y, X


def rms(v):
    return float(np.sqrt(np.mean(np.abs(v) ** 2)))


def _blocks(Z):
    """(n, 11, 64) output blocks of each row of Z (n, 64) placed at its own slot."""
    n = len(Z)
    B = np.zeros((n, 11, 64))
    for k in np.nonzero(np.abs(Z).sum(0))[0]:
        r = pslib._resp(k)
        B += Z[:, k].real[:, None, None] * r[0][None] + Z[:, k].imag[:, None, None] * r[1][None]
    return B


def slotfit(y, atoms, known=None, start=2 * 2048, end=None, ridge=0.0):
    """Per-slot real coefficients: y ~ synth(known) + sum_t sum_a c[t, a] synth(atom_a[t] at
    slot t). atoms: list of (n, 64) complex matrices. Returns (c (n, A), rms, max, residual)."""
    n = len(atoms[0])
    end = end or 64 * n - 640
    yy = np.array(y[:64 * n], float)
    if known is not None:
        yy = yy - psmodel.synthesis(known)
    m = np.zeros(64 * n + 11 * 64)
    m[start:end] = 1
    yy = np.concatenate([yy, np.zeros(11 * 64)]) * m
    M = np.lib.stride_tricks.sliding_window_view(m, 11 * 64)[::64][:n].reshape(n, 11, 64)
    Yw = np.lib.stride_tricks.sliding_window_view(yy, 11 * 64)[::64][:n].reshape(n, 11, 64)
    Bs = [_blocks(Z) * M for Z in atoms]
    A = len(atoms)
    G = np.zeros((n * A, n * A))
    rhs = np.zeros(n * A)
    for a in range(A):
        rhs[a::A] = np.einsum('tbk,tbk->t', Bs[a], Yw)
        for b_ in range(A):
            for s in range(11):
                v = np.einsum('tbk,tbk->t', Bs[a][:n - s, s:], Bs[b_][s:, :11 - s])
                idx = np.arange(n - s)
                G[idx * A + a, (idx + s) * A + b_] = v
                if s:
                    G[(idx + s) * A + b_, idx * A + a] = v
    used = np.diag(G) > 1e-30
    c = np.zeros(n * A)
    Gu = G[np.ix_(used, used)]
    if ridge:
        Gu = Gu + ridge * np.eye(len(Gu)) * np.mean(np.diag(Gu))
    c[used] = np.linalg.solve(Gu, rhs[used])
    c = c.reshape(n, A)
    model = np.zeros(64 * n + 11 * 64)
    for a in range(A):
        Z = atoms[a] * c[:, a:a + 1]
        model[:64 * n] += pslib.synth(Z[:, np.nonzero(np.abs(Z).sum(0))[0]], list(np.nonzero(np.abs(Z).sum(0))[0]))
    res = (yy - model * m)[start:end]
    return c, float(np.sqrt(np.mean(res ** 2))), float(np.abs(res).max()), res


def fitr(y, terms, known=None, start=2 * 2048, end=None, ridge=0.0, ret_res=False):
    """pslib.fit with an optional ridge (relative to the mean column energy)."""
    n = len(terms[0][1])
    end = end or 64 * n - 640
    yy = np.array(y[:64 * n], float)
    if known is not None:
        yy = yy - psmodel.synthesis(known)
    cols = []
    for k, s in terms:
        for c in (1.0, 1j):
            cols.append(psmodel.synth_band(c * s, k)[start:end])
    A = np.array(cols).T
    rhs = yy[start:end]
    G = A.T @ A
    if ridge:
        G = G + ridge * np.mean(np.diag(G)) * np.eye(len(G))
    a = np.linalg.solve(G, A.T @ rhs)
    r = rhs - A @ a
    o = (a[0::2] + 1j * a[1::2], float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max()))
    return o + (r,) if ret_res else o


# Parameter band of each QMF band above the hybrid-split ones, per configuration, as the
# hyb recovery found them (probes hyb/h01_onehot20, hyb/h03_onehot34): (first QMF band, band).
_QB = {
    20: [(3, 8), (4, 9), (5, 10), (6, 11), (7, 12), (8, 13), (9, 14), (11, 15), (14, 16),
         (18, 17), (23, 18), (35, 19)],
    34: [(5, 16), (6, 17), (7, 18), (8, 19), (9, 20), (10, 21), (11, 22), (13, 23), (15, 24),
         (17, 25), (19, 26), (21, 27), (24, 28), (27, 29), (30, 30), (33, 31), (37, 32),
         (41, 33)],
}
NLOW = {20: 3, 34: 5}


def qpar(cfg, k):
    b = None
    for k0, bb in _QB[cfg]:
        if k >= k0:
            b = bb
    return b


def qgroup(cfg, b):
    return [k for k in range(NLOW[cfg], 64) if qpar(cfg, k) == b]


def band_stream(path, cfg, k, NF=32, noise=0.01, seed=None, env_level=0, amp=30.0, payloads=None):
    """Tone at the centre of QMF band k plus band noise; ICC 7 in k's parameter band of
    configuration cfg (ICC mode 1 or 2), 0 elsewhere. Returns (y, X, grp)."""
    import dgen
    import sig
    b = qpar(cfg, k)
    grp = qgroup(cfg, b)
    lo, hi = (k - 1, k + 2) if len(grp) == 1 else (k + 0.15, k + 0.85)
    if seed is None:
        seed = 5 + k
    s = amp * (sig.tone(NF, sig.qmf_centre(k)) + sig.band_noise(NF, lo * sig.BW, hi * sig.BW, noise, seed=seed))
    if payloads is None:
        payloads = [icc_payload([b], nb=cfg)] * NF
    dgen.write_signal(path, s, payloads=payloads, env_level=env_level)
    return pslib.decode(path), pslib.dump(path)[0], grp
