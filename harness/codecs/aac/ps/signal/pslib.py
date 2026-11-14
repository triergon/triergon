"""Shared helpers of the Parametric Stereo signal-side recovery.

Streams come from harness/codecs/aac/ps/psgen.py. The reference (FFMPEG, the pinned 9.0.2
decode build) decodes them; this decoder's `ps_input` example dumps the QMF slots handed to
PS (PS_INPUT, by default target/release/examples/ps_input).

`fit` is the main tool: time-domain least squares of a reference output channel as the
64-band synthesis of known slots plus unknown complex coefficients times known slot
sequences (exact, since the synthesis is linear). `recover` (slots of a few bands from a
channel) is ill-conditioned outside each band's passband (p00); use it only for rough looks.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, '..', '..', '..', '..', '..'))
sys.path.insert(0, os.path.join(ROOT, 'harness', 'codecs', 'aac', 'ps'))
os.environ.setdefault('FFMPEG', os.path.join(ROOT, '.research', 'reference-decode', 'install', 'bin', 'ffmpeg'))
os.environ.setdefault('PS_INPUT', os.path.join(ROOT, 'target', 'release', 'examples', 'ps_input'))

import numpy as np  # noqa: E402

import psana  # noqa: E402
import psgen  # noqa: E402
import psmodel  # noqa: E402
import pssyntax as S  # noqa: E402

FFMPEG = os.environ['FFMPEG']


def books():
    """PS Huffman books as {name: {value: codeword}} from crates/codec-aac/src/huffman_probed.rs."""
    import re
    text = open(os.path.join(ROOT, 'crates', 'codec-aac', 'src', 'huffman_probed.rs')).read()
    out = {}
    off = {'IID_DF': 14, 'IID_DT': 14, 'IID_DF_FINE': 30, 'IID_DT_FINE': 30, 'ICC_DF': 7,
           'ICC_DT': 7, 'IPD_DF': 0, 'IPD_DT': 0, 'OPD_DF': 0, 'OPD_DT': 0}
    for name, o in off.items():
        m = re.search(r'static %s: \[\(u32, u8\); (\d+)\] = \[(.*?)\];' % name, text, re.S)
        pairs = re.findall(r'\((0x[0-9a-f]+|\d+), (\d+)\)', m.group(2))
        out[name] = {s - o: format(int(c, 0), '0%db' % int(n)) for s, (c, n) in enumerate(pairs)}
    return out


B = books()


def df_codes(idx, book):
    """Codewords of a list of absolute indices, frequency-differential."""
    prev = 0
    s = ''
    for v in idx:
        s += B[book][v - prev]
        prev = v
    return s


def dt_codes(idx, prev, book):
    return ''.join(B[book][v - p] for v, p in zip(idx, prev))


def ipd_df(idx, book='IPD_DF'):
    prev = 0
    s = ''
    for v in idx:
        s += B[book][(v - prev) % 8]
        prev = v
    return s


NB = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}


def payload(iid=None, icc=None, iid_mode=1, icc_mode=1, header=True, nenv=1, frame_class=0,
            borders=(), ext=None, iid_dt=None, icc_dt=None):
    """One ps_data() payload. iid/icc: per envelope a list of absolute indices (or one list for
    every envelope); None disables the parameter. ext: bits of the PS extension body (after
    cnt), or None for no extension."""
    s = ''
    if header:
        s = S.header(iid=None if iid is None else iid_mode, icc=None if icc is None else icc_mode,
                     ext=ext is not None)
    idx = {0: 0, 1: 1, 2: 2, 4: 3}[nenv] if frame_class == 0 else nenv - 1
    s += S.grid(frame_class, idx, borders)
    fine = iid_mode >= 3
    for name, val, dt in (('IID', iid, iid_dt), ('ICC', icc, icc_dt)):
        if val is None:
            continue
        envs = val if isinstance(val[0], (list, tuple)) else [val] * nenv
        book = name + ('_DF_FINE' if (name == 'IID' and fine) else '_DF')
        for e in range(nenv):
            s += '0' + df_codes(envs[e], book)
    if ext is not None:
        n = (len(ext) + 7) // 8
        body = ext + '0' * (8 * n - len(ext))
        s += format(min(n, 15), '04b') + (format(n - 15, '08b') if n >= 15 else '') + body
    return s


def ipdopd_ext(ipd, opd, nenv=1):
    """Body of PS extension id 0 with IPD/OPD (df coded) for every envelope."""
    s = '00' + '1'
    for e in range(nenv):
        s += '0' + ipd_df(ipd, 'IPD_DF') + '0' + ipd_df(opd, 'OPD_DF')
    return s + '0'


def write(path, payloads, seed=7, bins=None, **kw):
    """One frame per entry: a payload bit string, None (SBR without PS) or a list of
    (id, bits) extensions."""
    g = psgen.Gen(np.random.default_rng(seed), bins=bins, **kw)
    fr = []
    for p in payloads:
        if p is None:
            fr.append(g.frame([]))
        elif isinstance(p, list):
            fr.append(g.frame(p))
        else:
            fr.append(g.frame([(2, p)]))
    psgen.write_stream(path, fr)


def decode(path):
    return psana.decode(path)


def dump(path):
    """(frames*32, 64) slots handed to PS, and the (frames, 38, 64) raw dump."""
    x = psmodel.ps_input(path)
    return psmodel.slots(x), x


def band_bins(qbands, lo=4, hi=28):
    s = set()
    for q in qbands:
        s |= set(range(32 * q + lo, 32 * q + hi))
    return s


# --- exact recovery of a QMF matrix from a synthesised channel -------------------------

_RESP = {}


def _resp(k):
    """(2, 11, 64): output of a unit real / imaginary value in band k at slot 0."""
    if k not in _RESP:
        pr, pi = psmodel.band_response(k)
        _RESP[k] = np.array([pr, pi])
    return _RESP[k]


def synth(Y, bands):
    """Synthesis of the slots Y (n, len(bands)) complex placed in `bands`."""
    n = len(Y)
    out = np.zeros((n + 11, 64))
    for j, k in enumerate(bands):
        r = _resp(k)
        for b in range(11):
            out[b:b + n] += np.outer(Y[:, j].real, r[0, b]) + np.outer(Y[:, j].imag, r[1, b])
    return out[:n].reshape(-1)


def adjoint(y, bands, n):
    ym = np.zeros((n + 11, 64))
    ym[:n] = y[:64 * n].reshape(n, 64)
    Z = np.zeros((n, len(bands)), complex)
    for j, k in enumerate(bands):
        r = _resp(k)
        for b in range(11):
            Z[:, j] += ym[b:b + n] @ r[0, b] + 1j * (ym[b:b + n] @ r[1, b])
    return Z


def recover(y, bands, iters=300, tol=1e-13, start=0):
    """Least-squares QMF slots (n, len(bands)) whose synthesis is y (n*64 samples). Samples
    before `start` are ignored, and slots whose output lies wholly before it are zero."""
    n = len(y) // 64
    bands = list(bands)
    y = np.array(y[:64 * n], float)
    mask = np.ones(64 * n)
    mask[:start] = 0
    s0 = max(0, start // 64 - 10)

    def op(Z):
        Z = Z.copy()
        Z[:s0] = 0
        g = adjoint(mask * synth(Z, bands), bands, n)
        g[:s0] = 0
        return g

    b = adjoint(mask * y, bands, n)
    b[:s0] = 0
    x = np.zeros_like(b)
    r = b.copy()
    p = r.copy()
    rr = np.vdot(r, r).real
    b2 = rr
    for _ in range(iters):
        Ap = op(p)
        a = rr / np.vdot(p, Ap).real
        x += a * p
        r -= a * Ap
        rr2 = np.vdot(r, r).real
        if rr2 < tol * tol * b2:
            break
        p = r + (rr2 / rr) * p
        rr = rr2
    res = (y - synth(x, bands))[max(start, 640):-640]
    return x, float(np.sqrt(np.mean(res ** 2)))


def lstsq(A, y):
    a, *_ = np.linalg.lstsq(A, y, rcond=None)
    r = y - A @ a
    return a, float(np.sqrt(np.mean(np.abs(r) ** 2))), float(np.abs(r).max())


def shift(s, d):
    """s delayed by d slots (zeros shifted in)."""
    o = np.zeros_like(s)
    if d >= 0:
        o[d:] = s[:len(s) - d]
    else:
        o[:d] = s[-d:]
    return o


def recover_rest(y, X, target, known=None, **kw):
    """Recover the slots of the `target` bands from channel y after subtracting the synthesis
    of every other band, whose output slots are `known` (n, 64) (default: X itself)."""
    K = X if known is None else known
    rest = [k for k in range(64) if k not in target]
    base = synth(K[:, rest], rest)
    return recover(y[:len(base)] - base, target, **kw)


def fit(y, terms, known=None, start=2 * 2048, end=None):
    """Time-domain least squares of channel y as the synthesis of `known` (n, 64) slots (or
    nothing) plus one unknown complex coefficient per term. A term is (band k, slot sequence
    s): it contributes coef * s in band k. Rows from `start` to `end` are fitted. Returns
    (coefs, rms, max) of the residual."""
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
    a, rms, mx = lstsq(A, yy[start:end])
    return a[0::2] + 1j * a[1::2], rms, mx


def taps(s, lo, hi):
    """Terms for an FIR on sequence s with taps lo..hi (tap d multiplies s[t - d])."""
    return [shift(s, d) for d in range(lo, hi + 1)]
