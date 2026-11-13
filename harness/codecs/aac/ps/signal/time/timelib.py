"""Helpers of the time recovery: fits with one unknown per slot (or slot group).

A row term (t, V) is the QMF slot row V (64 complex values) placed at slot t; its output is
the 64-band synthesis of that single row, 11 slots long. A coefficient multiplies the sum
of its row terms. Because the synthesis is real-linear, every complex coefficient becomes
two real columns (or one, for a real coefficient)."""
import numpy as np

from pslib import *  # noqa: F401,F403

_R = None


def _resp():
    global _R
    if _R is None:
        R = np.zeros((2, 64, 11 * 64))
        for k in range(64):
            pr, pi = psmodel.band_response(k)
            R[0, k] = pr.reshape(-1)
            R[1, k] = pi.reshape(-1)
        _R = R
    return _R


def row_out(V):
    """(704,) output of the single slot row V at slot 0."""
    R = _resp()
    return V.real @ R[0] + V.imag @ R[1]


def place(n, t, w):
    """Output of length 64n with w (704 samples) starting at slot t (truncated)."""
    o = np.zeros(64 * n + 704)
    if t >= 0:
        o[64 * t:64 * t + len(w)] += w
    return o[:64 * n]


def column(n, rows):
    """Output (64n,) of a set of row terms [(t, V), ...]."""
    o = np.zeros(64 * n + 704)
    for t, V in rows:
        if 0 <= t < n:
            o[64 * t:64 * t + 704] += row_out(V)
    return o[:64 * n]


def fit_rows(y, known, unknowns, start, end, real=False):
    """Least squares of y[start:end] - synthesis(known) as sum_j c_j * column(rows_j).
    unknowns: list of row-term lists. Returns (coefs, rms, max, residual)."""
    n = len(known) if known is not None else (len(y) // 64)
    yy = np.array(y[:64 * n], float)
    if known is not None:
        yy = yy - psmodel.synthesis(known)
    cols = []
    for rows in unknowns:
        cols.append(column(n, rows)[start:end])
        if not real:
            cols.append(column(n, [(t, 1j * V) for t, V in rows])[start:end])
    A = np.array(cols).T
    a, *_ = np.linalg.lstsq(A, yy[start:end], rcond=None)
    r = yy[start:end] - A @ a
    c = a if real else a[0::2] + 1j * a[1::2]
    return c, float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max()), r


def masked(X, bands):
    """X with only the given bands kept."""
    Y = np.zeros_like(X)
    Y[:, list(bands)] = X[:, list(bands)]
    return Y


def slot_gain_fit(y, X, bands, slots, start, end, real=True, known=None, extra=()):
    """Per-slot gain g[t] (t in slots) of the bands `bands` of X; the remaining output is
    synthesis(known) (default: nothing). extra: further row-term lists fitted alongside."""
    unk = [[(t, masked(X[t:t + 1], bands)[0])] for t in slots] + list(extra)
    return fit_rows(y, known, unk, start, end, real=real)


def cl_cr(iid_db):
    c = 10 ** (np.asarray(iid_db, float) / 20)
    return np.sqrt(2) * c / np.sqrt(1 + c * c), np.sqrt(2) / np.sqrt(1 + c * c)


COARSE = [-25, -18, -14, -10, -7, -4, -2, 0, 2, 4, 7, 10, 14, 18, 25]
FINE = [-50, -45, -40, -35, -30, -25, -22, -19, -16, -13, -10, -8, -6, -4, -2, 0, 2, 4, 6, 8,
        10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50]
COSA = [1, 0.9841240, 0.9594738, 0.8946843, 0.8269341, 0.7071068, 0.4533211, 0]


def iid_db(idx, fine=False):
    return (FINE[idx + 15] if fine else COARSE[idx + 7])


# --- stream building with decoder messages ---------------------------------------------

def noheader(**kw):
    """A ps_data() payload without a header (enable_ps_header = 0)."""
    return '0' + payload(header=False, **kw)


def build(path, spec, seed=3, bins=None, **kw):
    """One frame per entry: a PS payload bit string, None (SBR without PS), a list of
    (id, bits) SBR extensions, or a dict of psgen.Gen.frame keyword arguments."""
    g = psgen.Gen(np.random.default_rng(seed), bins=bins, **kw)
    fr = []
    for p in spec:
        if p is None:
            fr.append(g.frame([]))
        elif isinstance(p, list):
            fr.append(g.frame(p))
        elif isinstance(p, dict):
            fr.append(g.frame(**p))
        else:
            fr.append(g.frame([(2, p)]))
    psgen.write_stream(path, fr)


def decode_log(path):
    """Reference output (n, 2) and the main decoder instance's messages."""
    import collections
    import re
    r = subprocess.run([FFMPEG, '-nostdin', '-loglevel', 'repeat+warning', '-f', 'aac', '-i', path,
                        '-f', 'f32le', '-c:a', 'pcm_f32le', '-'], capture_output=True)
    y = np.frombuffer(r.stdout, dtype='<f4').reshape(-1, 2).astype(np.float64)
    err = r.stderr.decode(errors='replace')
    lines = re.findall(r'^\[aac @ (0x[0-9a-f]+)\] (.*)$', err, re.M)
    main = collections.Counter(a for a, _ in lines).most_common(1)[0][0] if lines else None
    other = [l for l in err.splitlines() if not l.startswith('[aac @')]
    return y, [m for a, m in lines if a == main], other


def segments(g, tol=2e-5):
    """Compact text of a per-slot sequence: maximal linear runs as t0-t1:a>b (or t:a)."""
    out = []
    i = 0
    n = len(g)

    def lin(j):  # g[j-1], g[j], g[j+1] on a line
        return abs(g[j + 1] - 2 * g[j] + g[j - 1]) < tol

    while i < n:
        j = min(i + 1, n - 1)
        while j + 1 < n and lin(j):
            j += 1
        if j == i + 1 and j + 2 < n and lin(j + 1) and not lin(j):
            j = i  # i is an isolated point; the run starts at i + 1
        if j == i:
            out.append('%d:%.5f' % (i, g[i]))
        elif abs(g[j] - g[i]) < tol:
            out.append('%d-%d:%.5f' % (i, j, g[i]))
        else:
            out.append('%d-%d:%.5f>%.5f' % (i, j, g[i], g[j]))
        i = j + 1
    return ' '.join(out)


def gains(path, nf=None, show=True, label=''):
    """Per-slot real gains of the whole slot row in L and R (fitted), with the messages."""
    y, msgs, other = decode_log(path)
    X, _ = dump(path)
    n = len(X)
    gs = []
    stats = []
    per = []
    for c in (0, 1):
        g, rms, mx, r = slot_gain_fit(y[:, c], X, range(64), range(n), 0, 64 * (n - 11), real=True)
        gs.append(g)
        stats.append((rms, mx, float(np.sqrt(np.mean(y[:, c] ** 2)))))
        per.append([float(np.sqrt(np.mean(r[2048 * f:2048 * f + 2048] ** 2))) for f in range(len(r) // 2048)])
    if show:
        print('==', label or os.path.basename(path), 'frames', n // 32,
              'fit rms L %.2g R %.2g (signal %.2g %.2g)' % (stats[0][0], stats[1][0], stats[0][2], stats[1][2]))
        for m in msgs:
            print('   msg:', m)
        print('   residual rms per frame L:', ' '.join('%.1e' % v for v in per[0]))
        print('   residual rms per frame R:', ' '.join('%.1e' % v for v in per[1]))
        for f in range(n // 32 - 1):
            for c in (0, 1):
                print('  f%d %s' % (f, 'LR'[c]), segments(gs[c][32 * f:32 * f + 32]))
    return gs, y, X, msgs, stats


def pdata(iid=None, icc=None, iid_mode=1, icc_mode=1, header=True, fc=0, borders=(),
          iid_dt=(), icc_dt=(), prev_iid=None, prev_icc=None, nenv=None, ext=None):
    """ps_data() with per-envelope parameter lists. iid/icc: list of per-envelope index lists
    (None disables). iid_dt/icc_dt: envelope numbers coded time-differentially against
    prev_iid/prev_icc (the previous envelope's indices, given by the caller)."""
    envs = len(iid if iid is not None else icc) if nenv is None else nenv
    s = (S.header(iid=None if iid is None else iid_mode, icc=None if icc is None else icc_mode,
                  ext=ext is not None) if header else '0')
    idx = {0: 0, 1: 1, 2: 2, 4: 3}[envs] if fc == 0 else envs - 1
    s += S.grid(fc, idx, borders)
    fine = iid_mode >= 3
    for name, val, dts, prev in (('IID', iid, iid_dt, prev_iid), ('ICC', icc, icc_dt, prev_icc)):
        if val is None:
            continue
        for e in range(envs):
            if e in dts:
                book = name + ('_DT_FINE' if (name == 'IID' and fine) else '_DT')
                p = prev if e == 0 else val[e - 1]
                s += '1' + dt_codes(val[e], p, book)
            else:
                book = name + ('_DF_FINE' if (name == 'IID' and fine) else '_DF')
                s += '0' + df_codes(val[e], book)
    if ext is not None:
        n = (len(ext) + 7) // 8
        body = ext + '0' * (8 * n - len(ext))
        s += format(min(n, 15), '04b') + (format(n - 15, '08b') if n >= 15 else '') + body
    return s


class SbrGen(psgen.Gen):
    """psgen.Gen whose SBR payload bits can be altered per frame by a hook."""
    hook = None

    def sbr(self, extensions, declared=None, tail=''):
        bits = super().sbr(extensions, declared, tail)
        if self.hook is not None:
            bits = self.hook(bits)
            self.hook = None
        return bits


def build2(path, spec, seed=3, bins=None, **kw):
    """Like build; an entry may also be ('declared', bits, nbytes) (PS extension whose size
    field says nbytes), ('sbrhook', bits_or_None, fn) (SBR payload bits passed through fn),
    or ('nosbr',) (a frame without the SBR fill element)."""
    g = SbrGen(np.random.default_rng(seed), bins=bins, **kw)
    fr = []
    for p in spec:
        if isinstance(p, tuple):
            kind = p[0]
            if kind == 'declared':
                fr.append(g.frame([(2, p[1])], declared=p[2]))
            elif kind == 'sbrhook':
                g.hook = p[2]
                fr.append(g.frame([] if p[1] is None else [(2, p[1])]))
            elif kind == 'nosbr':
                fr.append(g.frame([], sbr=False))
            else:
                raise ValueError(kind)
        elif p is None:
            fr.append(g.frame([]))
        elif isinstance(p, list):
            fr.append(g.frame(p))
        else:
            fr.append(g.frame([(2, p)]))
    psgen.write_stream(path, fr)


QMF_PAR20 = [None, None, None, 8, 9, 10, 11, 12, 13, 14, 14] + [15] * 3 + [16] * 4 + [17] * 5 + [18] * 12 + [19] * 29


def group_gains(y, X, groups, n=None, real=True):
    """Per-slot real gains, one sequence per group of QMF bands, fitted jointly."""
    n = n or len(X)
    unk = []
    for gb in groups:
        unk += [[(t, masked(X[t:t + 1], gb)[0])] for t in range(n)]
    c, rms, mx, r = fit_rows(y, None, unk, 0, 64 * (n - 11), real=real)
    return [c[i * n:(i + 1) * n] for i in range(len(groups))], rms, mx, r
