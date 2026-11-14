"""Batch probe runner: many parameter payloads in one stream, each held for two frames,
measured on the second (steady) frame of its pair."""
import os
import numpy as np

import psgen
import psana

HOLD = 2


def consumed(path, payloads, seed=7):
    """Bits the reference's PS parser reads for each payload. Each payload goes in its own
    frame behind a size field of one byte (6 payload bits after the 2-bit id), with the
    whole payload written after it, so the parser reads past the declared end and the
    reference reports how far ("Expected to read 6 PS bits actually read M"). Every
    payload must read more than 6 bits. Returns a list of M."""
    import collections
    import re
    import subprocess
    g = psgen.Gen(np.random.default_rng(seed))
    # a payload given as ('normal', bits) is coded in a frame of its own size: it reads
    # no further than its extension and reports nothing (it sets state, e.g. for dt)
    frames = [g.frame([(2, p[1])]) if isinstance(p, tuple) else g.frame([(2, p)], declared=1)
              for p in payloads]
    psgen.write_stream(path, frames)
    r = subprocess.run([psana.FFMPEG, '-nostdin', '-loglevel', 'repeat+error', '-f', 'aac',
                        '-i', path, '-f', 'null', '-'], capture_output=True, text=True)
    lines = re.findall(r'^\[aac @ (0x[0-9a-f]+)\] (.*)$', r.stderr, re.M)
    # the stream probe decodes leading frames in a decoder instance of its own
    main = collections.Counter(a for a, _ in lines).most_common(1)[0][0]
    got = []
    for a, msg in lines:
        if a != main:
            continue
        m = re.match(r'Expected to read 6 PS bits actually read (\d+)\.', msg)
        got.append(int(m.group(1)) if m else msg)
    want = sum(not isinstance(p, tuple) for p in payloads)
    if len(got) != want:
        raise RuntimeError('%d reports for %d payloads:\n%s' % (len(got), want, r.stderr[:3000]))
    it = iter(got)
    return [None if isinstance(p, tuple) else next(it) for p in payloads]


def measure_frames(path, payloads, at, seed=7, **kw):
    """One frame per payload; returns (dB[64], coherence[64]) measured on the frames whose
    indices `at` lists (slots 4..27 of the frame)."""
    g = psgen.Gen(np.random.default_rng(seed), **kw)
    psgen.write_stream(path, [g.frame([(2, p)]) for p in payloads])
    y = psana.decode(path)
    L = psana.qmf_bands(y[:, 0])
    R = psana.qmf_bands(y[:, 1])
    res = []
    for f in at:
        s = slice(32 * f + 4, 32 * f + 28)
        el = (np.abs(L[s]) ** 2).sum(0)
        er = (np.abs(R[s]) ** 2).sum(0)
        c = (L[s] * R[s].conj()).sum(0) / np.sqrt(el * er + 1e-30)
        res.append((10 * np.log10((el + 1e-30) / (er + 1e-30)), c))
    return res


def run(path, payloads, seed=7, hold=HOLD, lead=None, extra=None, **kw):
    """Write one stream with each payload held for `hold` frames (after the optional
    `lead` payloads, one frame each), decode it and return per test:
    (dB[64], coherence[64], energy_l[64], energy_r[64]) measured on the last frame of the
    test. `extra` maps a test index to a list of further (id, bits) extensions."""
    g = psgen.Gen(np.random.default_rng(seed), **kw)
    frames = []
    for p in lead or []:
        frames.append(g.frame([(2, p)]))
    first = len(frames)
    for i, p in enumerate(payloads):
        ext = [(2, p)] + (extra.get(i, []) if extra else [])
        for _ in range(hold):
            frames.append(g.frame(ext))
    psgen.write_stream(path, frames)
    y = psana.decode(path)
    L = psana.qmf_bands(y[:, 0])
    R = psana.qmf_bands(y[:, 1])
    res = []
    for i in range(len(payloads)):
        f = first + i * hold + hold - 1
        s = slice(32 * f + 4, 32 * f + 28)
        el = (np.abs(L[s]) ** 2).sum(0)
        er = (np.abs(R[s]) ** 2).sum(0)
        c = (L[s] * R[s].conj()).sum(0) / np.sqrt(el * er + 1e-30)
        res.append((10 * np.log10((el + 1e-30) / (er + 1e-30)), c, el, er))
    return res, y
