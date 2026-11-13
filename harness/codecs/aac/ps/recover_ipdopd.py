"""Recover the PS IPD and OPD Huffman books and the IPD/OPD phase grid from the
reference's behaviour.

Extension layout (established by probes e20-e22): a 2-bit extension id; id 0 holds an
enable bit, then per envelope an IPD flag (0: frequency, 1: time differences) with one
codeword per IPD band, an OPD flag with one codeword per OPD band, and after the
envelopes one further bit. There are 5, 11 or 17 bands for IID modes 0/3, 1/4, 2/5.

1. Codewords. The reference reports by how many bits its parser read past a too-short
   extension ("ps extension overflow -N"), which gives the bits read inside it. For a
   tree node p in the first IPD (or OPD) band, the payload `... p 000...` makes the
   parser read the leftmost leaf below p and then only all-zero codewords, so the report
   gives that leaf's length less the all-zero codeword's length z. The depth-first
   enumeration is run for each z from 1 to 5: only one gives a consistent complete
   prefix code.
2. Values. With IID and ICC at index 0, the phases arg(L R*) (IPD) and arg(L M*) (OPD,
   M the output of the same stream with neutral PS data) are measured in QMF bands 3 and 4
   (20-band parameter bands 8 and 9; band 10 borders band 11, which has no IPD) after the values have been held for eight
   frames. df: codeword c in every band gives the indices (b + 1) v at band b, so the
   bands 8..10 give v. dt: the codeword that keeps every phase at 0 when repeated is the
   zero; then a frame of c in every band after a df frame at index 0, held by dt zeros,
   gives v.
3. Checks: every index is a multiple of one phase step (the grid); df sums beyond the
   largest index are accepted and wrap modulo the number of indices; dt values are
   checked a second time after a df frame at a non-zero base.

usage: recover_ipdopd.py OUT.json WORKDIR
"""
import json
import os
import sys
from fractions import Fraction

import numpy as np

import ipdprobe as X

N = 11          # IPD/OPD bands in IID mode 1
HOLD = 8
ZBITS = '0' * 400


def content(ipd, opd, fi='0', fo='0'):
    return '001' + fi + ''.join(ipd) + fo + ''.join(opd) + '0'


def payload(e):
    cnt = (len(e) + 7) // 8
    return X.payload(e + '0' * (cnt * 8 - len(e)), iid=1, icc=1, cnt=cnt,
                     iid_codes=['0' + '0' * 20], icc_codes=['0' + '0' * 20])


def dfs(work, head, z, tag):
    """Leaves of the book read right after `head` (extension bits), assuming its all-zero
    codeword has z bits; None when z is inconsistent."""
    leaves, frontier, rnd = [], [''], 0
    while frontier and rnd < 40:
        rnd += 1
        items = [(X.payload(head + p + ZBITS, iid=1), 1) for p in [''] + frontier]
        got = X.ext_read(os.path.join(work, 'dfs-%s-%d' % (tag, z)), items)
        t0 = got[0]
        nxt = []
        for p, g in zip(frontier, got[1:]):
            if not isinstance(g, int):
                return None
            L = g - t0 + z
            leaf = p + '0' * (L - len(p))
            if L < len(p) or L > 24 or leaf in leaves:
                return None
            leaves.append(leaf)
            nxt += [leaf[:d] + '1' for d in range(len(p), L)]
        frontier = nxt
    if sum(Fraction(1, 2 ** len(c)) for c in leaves) != 1:
        return None
    return sorted(leaves, key=lambda c: (len(c), c))


def book_codes(work, which, flag, zero_ipd_df):
    if which == 'ipd':
        head = '001' + flag
    else:
        head = '001' + '0' + zero_ipd_df * N + flag
    found = {z: dfs(work, head, z, which + flag) for z in range(1, 6)}
    ok = {z: c for z, c in found.items() if c}
    assert len(ok) == 1, (which, flag, {z: bool(c) for z, c in found.items()})
    (z, codes), = ok.items()
    assert codes[0] == '0' * z or '0' * z in codes
    return codes


def measure(work, frames, at, tag):
    res = X.phases(os.path.join(work, 'm-' + tag), [payload(f) for f in frames], at,
                   X.NEUTRAL)
    return [(r[0][3:5], r[1][3:5]) for r in res]


def main():
    out, work = sys.argv[1], sys.argv[2]
    os.makedirs(work, exist_ok=True)
    books = {}
    books['ipd_df'] = book_codes(work, 'ipd', '0', None)
    zdf = next(c for c in books['ipd_df'] if set(c) == {'0'})
    books['ipd_dt'] = book_codes(work, 'ipd', '1', None)
    books['opd_df'] = book_codes(work, 'opd', '0', zdf)
    books['opd_dt'] = book_codes(work, 'opd', '1', zdf)
    for b, c in books.items():
        print(b, len(c), c)

    # df values. arg(L R*) follows the IPD alone and arg(L M*) the OPD alone (probe e23),
    # so the other block holds its all-zero codeword, whatever its value.
    zodf = next(c for c in books['opd_df'] if set(c) == {'0'})

    def df_frames(kind, codes):
        if kind == 'ipd':
            return content(codes, [zodf] * N)
        return content([zdf] * N, codes)

    # (a) the df zero: the codeword that, in every band, leaves every phase at 0 (any
    # other value v gives the indices (b + 1) v at band b, which differ between bands)
    frames, at, rows = [], [], []
    for kind in ('ipd', 'opd'):
        for c in books[kind + '_df']:
            frames += [df_frames(kind, [c] * N)] * HOLD
            at.append(len(frames) - 1)
            rows.append((kind, c))
    res = measure(work, frames, at, 'dfzero')
    zero = {}
    for (kind, c), (lr, lm) in zip(rows, res):
        p = lr if kind == 'ipd' else lm
        if np.abs(p).max() < 1e-3:
            zero.setdefault(kind, []).append(c)
    assert all(len(zero[k]) == 1 for k in ('ipd', 'opd')), zero
    zi, zo = zero['ipd'][0], zero['opd'][0]
    # (b) each codeword in band 0 and the zero after it: every band at index v, so the
    # measured phase is one grid value
    frames, at, rows = [], [], []
    for kind in ('ipd', 'opd'):
        z = zi if kind == 'ipd' else zo
        for c in books[kind + '_df']:
            frames += [df_frames(kind, [c] + [z] * (N - 1))] * HOLD
            at.append(len(frames) - 1)
            rows.append((kind, c))
    res = measure(work, frames, at, 'df')
    raw = {}
    for (kind, c), (lr, lm) in zip(rows, res):
        p = lr if kind == 'ipd' else lm
        assert np.ptp(np.unwrap(p)) < 1e-3, (kind, c, p)
        raw[(kind, c)] = float(np.angle(np.exp(1j * p).mean()))
    nz = sorted(abs(x) for x in raw.values() if abs(x) > 1e-3)
    step = nz[0]
    nidx = int(round(2 * np.pi / step))
    assert abs(nidx * step - 2 * np.pi) < 1e-3, (step, nidx)
    values = {}
    for (kind, c), x in raw.items():
        v = int(round(x / step)) % nidx
        values.setdefault(kind + '_df', {})[c] = v
    for kind in ('ipd', 'opd'):
        vals = sorted(values[kind + '_df'].values())
        assert vals == list(range(nidx)), (kind, vals)

    # dt zero: repeated in every frame, keeps every phase at 0
    frames, at, rows = [], [], []
    for kind in ('ipd', 'opd'):
        for c in books[kind + '_dt']:
            lead = content([zi] * N, [zo] * N)
            if kind == 'ipd':
                e = content([c] * N, [zo] * N, fi='1')
            else:
                e = content([zi] * N, [c] * N, fo='1')
            frames += [lead] * HOLD + [e] * 4
            at += list(range(len(frames) - 4, len(frames)))
            rows.append((kind, c))
    res = measure(work, frames, at, 'dtzero')
    zdt = {}
    for i, (kind, c) in enumerate(rows):
        ps = [r[0] if kind == 'ipd' else r[1] for r in res[4 * i:4 * i + 4]]
        if all(np.abs(p).max() < 1e-3 for p in ps):
            zdt.setdefault(kind, []).append(c)
    assert all(len(zdt[k]) == 1 for k in ('ipd', 'opd')), zdt
    zti, zto = zdt['ipd'][0], zdt['opd'][0]

    # dt values, at base 0 and at base 3 (a df frame of the value-3 codeword in band 0)
    v3 = {k: next(c for c, v in values[k + '_df'].items() if v == 3) for k in ('ipd', 'opd')}
    frames, at, rows = [], [], []
    for base in (0, 3):
        for kind in ('ipd', 'opd'):
            for c in books[kind + '_dt']:
                bi = [v3['ipd']] + [zi] * (N - 1) if base else [zi] * N
                bo = [v3['opd']] + [zo] * (N - 1) if base else [zo] * N
                lead = content(bi, bo)
                if kind == 'ipd':
                    e = content([c] * N, [zto] * N, fi='1', fo='1')
                    hold = content([zti] * N, [zto] * N, fi='1', fo='1')
                else:
                    e = content([zti] * N, [c] * N, fi='1', fo='1')
                    hold = content([zti] * N, [zto] * N, fi='1', fo='1')
                frames += [lead] * HOLD + [e] + [hold] * HOLD
                at.append(len(frames) - 1)
                rows.append((base, kind, c))
    res = measure(work, frames, at, 'dt')
    for (base, kind, c), (lr, lm) in zip(rows, res):
        p = lr if kind == 'ipd' else lm
        idx = [int(round(x / step)) % nidx for x in p]
        assert len(set(idx)) == 1, (base, kind, c, p)
        for x in p:
            assert abs(np.angle(np.exp(1j * (x - step * idx[0])))) < 2e-3, (base, kind, c, p)
        v = (idx[0] - base) % nidx
        d = values.setdefault(kind + '_dt', {})
        assert d.get(c, v) == v, (kind, c, d.get(c), v, base)
        d[c] = v
    for kind in ('ipd', 'opd'):
        vals = sorted(values[kind + '_dt'].values())
        assert vals == list(range(nidx)), (kind, vals)

    # df wrap: two codewords of the largest value in bands 0 and 1 are accepted and put
    # bands 1.. at (2 (nidx - 1)) mod nidx
    v7 = {k: next(c for c, v in values[k + '_df'].items() if v == nidx - 1) for k in ('ipd', 'opd')}
    e = content([v7['ipd']] * 2 + [zi] * (N - 2), [v7['opd']] * 2 + [zo] * (N - 2))
    got = X.ext_read(os.path.join(work, 'wrap'), [(payload(e), (len(e) + 7) // 8)])[0]
    assert isinstance(got, tuple) and got[0] == 'fits', got
    res = measure(work, [e] * HOLD, [HOLD - 1], 'wrap')[0]
    want = np.angle(np.exp(1j * step * ((2 * (nidx - 1)) % nidx)))
    for p in res:
        assert np.abs(np.angle(np.exp(1j * (p - want)))).max() < 2e-3, ('wrap', p, want)
    wrap = {'ipd_bands_8_9': [float(x) for x in res[0]],
            'opd_bands_8_9': [float(x) for x in res[1]], 'expected': float(want)}

    # the grid: every steady df measurement against its multiple of the step
    devs = [abs(np.angle(np.exp(1j * (x - step * values[k + '_df'][c]))))
            for (k, c), x in raw.items()]
    result = {
        'layout': 'id(2) | enable(1) | per envelope: ipd_dt(1) ipd[n] opd_dt(1) opd[n] | 1 bit',
        'bands': {'iid_mode_0_3': 5, 'iid_mode_1_4': 11, 'iid_mode_2_5': 17},
        'indices': nidx, 'phase_step': step, 'phase_step_over_pi': step / np.pi,
        'grid_max_deviation_rad': float(max(devs)),
        'books': {b: {c: values[b][c] for c in books[b]} for b in books},
        'zero_codewords': {'ipd_df': zi, 'opd_df': zo, 'ipd_dt': zti, 'opd_dt': zto},
        'df_wrap': wrap,
    }
    for b in books:
        k = sum(Fraction(1, 2 ** len(c)) for c in books[b])
        assert k == 1 and len(books[b]) == nidx
    json.dump(result, open(out, 'w'), indent=1)
    print(json.dumps(result, indent=1))


if __name__ == '__main__':
    main()
