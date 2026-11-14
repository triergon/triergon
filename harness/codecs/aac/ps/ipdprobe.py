"""Probe helpers for the PS extension (IPD/OPD) recovery.

`ext_read` reports how many bits the reference's parser read inside a PS extension:
each payload declares an extension size of `cnt` bytes but holds more, so the parser
reads past it and the reference reports by how much ("ps extension overflow -N"), which
gives 8 * cnt + N bits read. Payloads come from `payload` (pssyntax order: header with
enable_ext, grid, IID/ICC data, the 4-bit extension size, then the extension bits).
"""
import os

import numpy as np

import psana
import psgen
import psprobe
import pssyntax as S


def payload(ext_bits, iid=None, icc=None, envs=1, frame_class=0, borders=(), cnt=1,
            iid_codes=None, icc_codes=None):
    """A ps_data() payload with zero codewords ('0', the coarse IID / ICC df zero) for the
    IID / ICC bands unless `iid_codes` / `icc_codes` give them per envelope."""
    s = S.header(iid=iid, icc=icc, ext=True)
    if frame_class:
        s += S.grid(1, envs - 1, borders)
    else:
        s += S.grid(0, {1: 1, 2: 2, 4: 3}[envs])
    for name, mode, codes in (('iid', iid, iid_codes), ('icc', icc, icc_codes)):
        if mode is None:
            continue
        nb = S.NBANDS[mode]
        for e in range(envs):
            if codes is None:
                zero = '1' if (name == 'iid' and mode >= 3) else '0'
                s += '0' + zero * nb
            else:
                s += codes[e]
    if cnt >= 15:
        s += '1111' + format(cnt - 15, '08b')
    else:
        s += format(cnt, '04b')
    return s + ext_bits


def ext_read(path, items):
    """items: (payload, cnt). Returns bits read inside the extension, or the message."""
    import re
    got = psprobe.consumed(path, [p + '0' * 64 for p, _ in items])
    out = []
    for (p, cnt), g in zip(items, got):
        if isinstance(g, str):
            m = re.match(r'ps extension overflow (-?\d+)', g)
            out.append(8 * cnt - int(m.group(1)) if m else g)
        else:
            out.append(('fits', g))
    return out


def phases(path, payloads, at, neutral_payload, seed=7):
    """Decode `payloads` (one frame each) and the same stream with every payload replaced
    by `neutral_payload`; returns per measured frame (arg L R*, arg L M*, arg R M*,
    |L|/|M| energy ratio) per QMF band, over slots 4..27."""
    out = []
    ys = []
    for pays in (payloads, [neutral_payload] * len(payloads)):
        g = psgen.Gen(np.random.default_rng(seed))
        p = path + ('.n.aac' if ys else '.aac')
        psgen.write_stream(p, [g.frame([(2, q)]) for q in pays])
        ys.append(psana.decode(p))
    y, n = ys
    L = psana.qmf_bands(y[:, 0])
    R = psana.qmf_bands(y[:, 1])
    M = psana.qmf_bands(n[:, 0])
    for f in at:
        s = slice(32 * f + 4, 32 * f + 28)
        lr = (L[s] * R[s].conj()).sum(0)
        lm = (L[s] * M[s].conj()).sum(0)
        rm = (R[s] * M[s].conj()).sum(0)
        el = (np.abs(L[s]) ** 2).sum(0)
        em = (np.abs(M[s]) ** 2).sum(0)
        out.append((np.angle(lr), np.angle(lm), np.angle(rm), el / (em + 1e-30)))
    return out


NEUTRAL = S.header() + S.grid(0, 1)
