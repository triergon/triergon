"""Recover the SBR frequency-direction 3.0 dB balance book (coupled channel pair) from the
reference's behaviour.

Probe frames (sbrcpe): a coupled pair, two FIXFIX envelopes at low resolution (five bands,
so four frequency-delta codewords per envelope, coded with the 3.0 dB books), two noise
bands. The all-zero data run of the pair parses as 62 bits (probes/e43), so the all-zero
codeword of both the level channel's and the balance channel's frequency-delta books is
'0'; the level channel is all zero bits throughout.

Length oracle: the data is followed by a tail (add_harmonic 0 0, bs_extended_data 1, one
byte, id 2, a PS payload that reads 7 bits behind a 6-bit size), and the reference reports
"Expected to read 6 PS bits actually read 7" only when its SBR parser reaches the tail
exactly. The codeword under test codes the last band of the balance channel's second
envelope; then come zero bits and the tail. For a tree node p, the gap of zero bits after p
at which the report appears gives the length of the leftmost leaf below p (p's zero
extension) plus the 12 bits of the zero noise data. A second tail (a PS payload reading 10
bits) confirms each length.

Values: the second envelope's 5-bit start value s sets the base 2s for its bands; the
reference refuses a frame whose balance value leaves its range (the parse then stops before
the tail). Each codeword is accepted for a contiguous range of s, which gives its value up
to one common offset; the offset is fixed by the codeword whose levels stay flat (value 0)
in a level measurement of the balance channel's high band.

Independent check: the same codewords are found again, with the same lengths and values,
as the frequency delta of the balance channel's second noise floor.

usage: recover_sbr_envbal.py OUT.json WORKDIR
"""
import json
import os
import sys
from fractions import Fraction

import numpy as np

import psana
import sbrcpe
import sbrprobe

CH0 = '0' * 32
TAIL7 = '00' + '1' + '0001' + '10' + '1000000' + '0' * 48
TAIL10 = '00' + '1' + '0001' + '10' + '1100000000' + '0' * 48
GMAX = 40


def ch1_env(p, s2=0, s1=0):
    """Balance channel data with p as the last frequency delta of envelope 2."""
    return (format(s1, '05b') + '0' * 4 + format(s2, '05b') + '0' * 3 + p, 12)


def ch1_noise(p, s2=0, s1=0):
    """Balance channel data with p as the frequency delta of noise floor 2 (start s2)."""
    env = ('00000' + '0' * 4) * 2
    return (env + format(s1, '05b') + '0' + format(s2, '05b') + p, 0)


def lengths(nodes, ctx, s2, work, tag):
    """Leftmost-leaf length below each node p (None when no gap gives the report)."""
    tests, meta = [], []
    for p in nodes:
        body, after = ctx(p, s2)
        for g in range(0, GMAX):
            tests.append((CH0, body + '0' * g, TAIL7))
            meta.append((p, g, after))
    res = sbrprobe.messages(os.path.join(work, '%s.aac' % tag), tests)
    hits = {}
    for (p, g, after), m in zip(meta, res):
        if sbrprobe.ps_read(m) == 7:
            hits.setdefault(p, []).append(g)
    out = {}
    check = []
    for p in nodes:
        gs = hits.get(p, [])
        if len(gs) == 1:
            out[p] = len(p) + gs[0] - ctx(p, s2)[1]
            check.append((p, gs[0]))
        else:
            out[p] = None if not gs else ('ambiguous', gs)
    # confirm with the second tail
    if check:
        tests = [(CH0, ctx(p, s2)[0] + '0' * g, TAIL10) for p, g in check]
        res = sbrprobe.messages(os.path.join(work, '%s-c.aac' % tag), tests)
        for (p, g), m in zip(check, res):
            assert sbrprobe.ps_read(m) == 10, (p, g, m)
    return out


def enumerate_book(ctx, work, tag, bases):
    leaves, frontier, refused, rnd = {}, [''], [], 0
    while frontier:
        rnd += 1
        pending = list(frontier)
        found = {}
        for s2 in bases:
            if not pending:
                break
            got = lengths(pending, ctx, s2, work, '%s-%d-s%d' % (tag, rnd, s2))
            for p, L in got.items():
                if isinstance(L, int):
                    found[p] = L
                elif isinstance(L, tuple):
                    raise RuntimeError('ambiguous gap for %s: %s' % (p, L))
            pending = [p for p in pending if p not in found]
        refused += pending
        nxt = []
        for p, L in found.items():
            assert len(p) <= L <= 24, (p, L)
            leaf = p + '0' * (L - len(p))
            leaves[leaf] = L
            nxt += [leaf[:d] + '1' for d in range(len(p), L)]
        print('%s round %d: %d leaves, %d next, %d refused' % (tag, rnd, len(leaves), len(nxt), len(refused)))
        frontier = nxt
    return sorted(leaves, key=lambda c: (len(c), c)), refused


def accepted_bases(codes, ctx, work, tag):
    """For each codeword, the start values s2 (0..31) after which the frame reaches the
    tail."""
    tests, meta = [], []
    for c in codes:
        for s2 in range(32):
            body, after = ctx(c, s2)
            tests.append((CH0, body + '0' * after, TAIL7))
            meta.append((c, s2))
    res = sbrprobe.messages(os.path.join(work, tag + '-range.aac'), tests)
    acc = {}
    for (c, s2), m in zip(meta, res):
        if sbrprobe.ps_read(m) == 7:
            acc.setdefault(c, []).append(s2)
    return acc


def relative_values(acc):
    """Value of each codeword relative to the zero-extension offset: a codeword of value v
    is accepted for 2*s2 + v in the common range [lo, hi]; v = 2*(s_ref) - 2*s for the
    upper ends when the range is cut there, the lower ends otherwise."""
    lows = {c: min(s) for c, s in acc.items()}
    highs = {c: max(s) for c, s in acc.items()}
    for c, s in acc.items():
        assert s == list(range(min(s), max(s) + 1)), (c, s)
    smax = max(highs.values())
    smin = min(lows.values())
    rel = {}
    for c in acc:
        if highs[c] < smax:
            rel[c] = ('hi', 2 * (smax - highs[c]))
        elif lows[c] > smin:
            rel[c] = ('lo', -2 * (lows[c] - smin))
        else:
            rel[c] = ('both', 0)
    return rel, lows, highs


def balance_levels(codes, work, s2=6):
    """L/R level ratio (dB) of the high band in each of the five envelope bands, envelope
    2, with codeword c as the last delta and start value s2 (and envelope 1 the same)."""
    g = sbrcpe.Gen(np.random.default_rng(5))
    frames = []
    for c in codes:
        body = format(s2, '05b') + '0' * 3 + c
        ch1 = body + body + ('00000' + '0') * 2
        f = g.frame(CH0, ch1, '000')
        frames += [f, g.frame(CH0, ch1, '000')]
    path = os.path.join(work, 'levels.aac')
    sbrcpe.stream(path, frames)
    y = psana.decode(path)
    L = psana.qmf_bands(y[:, 0])
    R = psana.qmf_bands(y[:, 1])
    borders = [32, 36, 42, 48, 56, 64]
    out = []
    for i in range(len(codes)):
        f = 2 * i + 1
        s = slice(32 * f + 4, 32 * f + 28)
        db = []
        for b in range(5):
            el = (np.abs(L[s, borders[b] + 1:borders[b + 1] - 1]) ** 2).sum()
            er = (np.abs(R[s, borders[b] + 1:borders[b + 1] - 1]) ** 2).sum()
            db.append(10 * np.log10(el / er))
        out.append(db)
    return out


def main():
    out, work = sys.argv[1], sys.argv[2]
    os.makedirs(work, exist_ok=True)
    result = {}
    for name, ctx in (('envelope', ch1_env), ('noise', ch1_noise)):
        codes, refused = enumerate_book(ctx, work, name, [0, 6, 12, 3, 9, 15, 20, 25, 31])
        k = sum(Fraction(1, 2 ** len(c)) for c in codes)
        print(name, 'codewords', len(codes), 'kraft', k, 'refused', refused)
        acc = accepted_bases(codes, ctx, work, name)
        rel, lows, highs = relative_values(acc)
        result[name] = {'codes': codes, 'kraft': str(k), 'refused': refused,
                        'accepted': {c: [lows[c], highs[c]] for c in codes}, 'relative': rel}
    env, noise = result['envelope'], result['noise']
    assert env['codes'] == noise['codes'], 'contexts found different codewords'
    codes = env['codes']
    # values from the noise context, where the reference bounds the balance noise floor
    # on both sides: accepted start values s in [lo, hi] for 0 <= 2s + v <= top
    nacc = noise['accepted']
    # the zero codeword ('0', the all-zero parse of e43) fixes the range: 0 <= 2s <= top
    zlo, zhi = nacc['0']
    assert zlo == 0
    top = 2 * zhi
    values = {}
    for c in codes:
        lo, hi = nacc[c]
        v = -2 * lo if lo > 0 else top - 2 * hi
        # both ends must agree with 0 <= 2s + v <= top (within the 5-bit start value)
        assert (lo, hi) == (max(0, -(v // 2)), min(31, (top - v) // 2)), (c, lo, hi, v)
        values[c] = v
    vs = sorted(values.values())
    print('values', vs)
    # check 1: the envelope context bounds the balance envelope from below only; its
    # lower bounds give the negative values again
    eacc = env['accepted']
    env_ok = all((eacc[c][0] == -values[c] // 2) if values[c] < 0 else eacc[c][0] == 0 for c in codes)
    print('envelope context lower bounds agree:', env_ok)
    # check 2: measured level step from band 4 to band 5 of the balance channel's
    # envelopes, from a start value that centres the two bands on the balanced level; the
    # steps must rise with the values and the zero codeword's must vanish
    steps = {}
    by_base = {}
    for c in codes:
        by_base.setdefault(max(0, min(31, 6 - (values[c] // 2) // 2)), []).append(c)
    for s2, group in sorted(by_base.items()):
        for c, l in zip(group, balance_levels(group, work, s2)):
            steps[c] = l[4] - l[3]
    # the measurement saturates near +-48 dB, so the steps must rise up to |value| 18 and
    # lie on one line up to |value| 8, and every step has its value's sign
    order = sorted(codes, key=lambda c: values[c])
    mid = [c for c in order if abs(values[c]) <= 18]
    lin = [c for c in order if abs(values[c]) <= 8]
    slope = sum(steps[c] * values[c] for c in lin) / sum(values[c] ** 2 for c in lin)
    resid = max(abs(steps[c] - slope * values[c]) for c in lin)
    print('dB per balance step %.4f, worst residual %.3f dB' % (slope, resid))
    rising = bool(all(steps[a] < steps[b] for a, b in zip(mid, mid[1:])) and resid < 0.5
              and all((steps[c] > 0) == (values[c] > 0) for c in codes if values[c]))
    print('level steps by value (dB):', [(values[c], round(steps[c], 2)) for c in order])
    print('steps rise with the values:', rising)
    json.dump({'book': 'sbr_f_envbal30',
               'codewords': {c: values[c] // 2 for c in codes},
               'value_unit': 'symbol delta; the decoded balance step is twice it',
               'checks': {'envelope_lower_bounds': env_ok,
                          'level_steps_db': {c: float(steps[c]) for c in codes},
                          'db_per_balance_step': float(slope), 'worst_residual_db': float(resid),
                          'steps_rise_with_values': rising},
               'contexts': result}, open(out, 'w'), indent=1)
    k = sum(Fraction(1, 2 ** len(c)) for c in codes)
    assert k == 1 and not env['refused'] and not noise['refused'], 'incomplete book'
    assert vs == list(range(-24, 25, 2)), 'values not the 25 steps -24..24'
    assert env_ok, 'envelope context disagrees'
    assert rising and abs(steps['0']) < 0.1, 'levels disagree'


if __name__ == '__main__':
    main()
