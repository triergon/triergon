"""Parametric fits of a per-band decorrelator response (decorrelator recovery): a chain of first-order
sections in z^-d, S(z) = (alpha + z^-d) / (1 + beta z^-d), times G z^-d0."""
import numpy as np
import psmodel
from pslib import shift


def section(x, d, alpha, beta):
    """y[t] = alpha x[t] + x[t - d] - beta y[t - d] (zero initial state)."""
    n = len(x)
    v = alpha * x + shift(x, d)
    y = np.zeros(n, complex)
    for t0 in range(0, n, d):
        t1 = min(n, t0 + d)
        prev = y[t0 - d:t1 - d] if t0 >= d else np.zeros(t1 - t0, complex)
        y[t0:t1] = v[t0:t1] - beta * prev
    return y


def chain(x, d0, G, secs):
    y = G * shift(x, d0)
    for d, a, b in secs:
        y = section(y, d, a, b)
    return y


def unpack(p, ds):
    G = p[0] + 1j * p[1]
    secs = [(d, p[2 + 4 * i] + 1j * p[3 + 4 * i], p[4 + 4 * i] + 1j * p[5 + 4 * i]) for i, d in enumerate(ds)]
    return G, secs


def gn(target, x, k, d0, ds, p0, rows, iters=30, gain=None, verbose=False):
    """Gauss-Newton fit of the chain parameters: target ~ synth_band(gain * chain(x), k)."""
    p = np.array(p0, float)

    def model(p):
        G, secs = unpack(p, ds)
        D = chain(x, d0, G, secs)
        if gain is not None:
            D = D * gain
        return psmodel.synth_band(D, k)[rows]

    lam = 1e-3
    r = target - model(p)
    cost = r @ r
    for it in range(iters):
        J = np.empty((len(r), len(p)))
        for i in range(len(p)):
            e = np.zeros(len(p))
            e[i] = 1e-7
            J[:, i] = (model(p + e) - model(p - e)) / 2e-7
        JTJ = J.T @ J
        g = J.T @ r
        while True:
            step = np.linalg.lstsq(JTJ + lam * np.diag(np.diag(JTJ)), g, rcond=None)[0]
            r2 = target - model(p + step)
            c2 = r2 @ r2
            if c2 < cost:
                p, r, cost = p + step, r2, c2
                lam = max(lam / 10, 1e-12)
                break
            lam *= 10
            if lam > 1e8:
                return p, np.sqrt(cost / len(r))
        if verbose:
            print('   it %d rms %.4g' % (it, np.sqrt(cost / len(r))))
        if np.abs(step).max() < 1e-12:
            break
    return p, np.sqrt(cost / len(r))


def ap_section(x, d, a, Q):
    """All-pass section (-a + Q z^-d) / (1 - a Q z^-d), real a, |Q| = 1."""
    return section(x, d, -a * np.conj(Q), -a * Q) * Q


def ap_chain(x, d0, G, secs):
    """G z^-d0 prod over (d, a, Q) of ap_section."""
    y = G * shift(x, d0)
    for d, a, Q in secs:
        y = ap_section(y, d, a, Q)
    return y


def gn_generic(target, model, p0, iters=40, h=1e-7, verbose=False):
    p = np.array(p0, float)
    lam = 1e-3
    r = target - model(p)
    cost = r @ r
    for it in range(iters):
        J = np.empty((len(r), len(p)))
        for i in range(len(p)):
            e = np.zeros(len(p))
            e[i] = h
            J[:, i] = (model(p + e) - model(p - e)) / (2 * h)
        JTJ = J.T @ J
        g = J.T @ r
        ok = False
        while lam < 1e8:
            Mx = JTJ + lam * np.diag(np.diag(JTJ)) + 1e-300 * np.eye(len(p))
            step = np.linalg.lstsq(Mx, g, rcond=None)[0]
            r2 = target - model(p + step)
            c2 = r2 @ r2
            if c2 < cost:
                p, r, cost = p + step, r2, c2
                lam = max(lam / 10, 1e-12)
                ok = True
                break
            lam *= 10
        if verbose:
            print('   it %d rms %.4g' % (it, np.sqrt(cost / len(r))))
        if not ok or np.abs(step).max() < 1e-13:
            break
    cov = None
    try:
        cov = np.linalg.pinv(JTJ) * cost / max(1, len(r) - len(p))
    except Exception:
        pass
    return p, float(np.sqrt(cost / len(r))), cov
