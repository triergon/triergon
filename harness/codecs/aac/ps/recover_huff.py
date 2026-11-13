"""Recover a PS Huffman book (codewords and values) from the reference's behaviour.

The reference reports how many bits its PS parser read when that exceeds the declared
extension size (psprobe.consumed). The probes code one envelope of 20 parameter bands;
the codeword under test codes the last band, after codewords already known.

1. The book's zero codeword: a string s whose nb-fold repetition the parser reads as
   exactly nb codewords (nb * len(s) bits) is a codeword; the one whose decoded value
   is 0 (levels unchanged) is kept.
2. Codewords: a book is a complete prefix code, so it is enumerated depth first. For a
   tree node p, the payload `... p 000...` makes the parser read the leftmost leaf below
   p (nothing follows the last band), so the report gives that leaf's length; every
   sibling branch found on the way is a further node.
3. Values: each codeword's band is measured (IID: the L/R level ratio in dB; ICC: the L/R
   normalised correlation) on a held frame, and mapped to an index by the order of the
   levels found from index 0. A codeword the reference refuses (index out of range) is
   retried with the preceding bands raised or lowered to the edge of the range by known
   codewords.

usage: recover_huff.py BOOK OUT.json WORKDIR [DF.json]
  BOOK: iid_df, iid_dt, iid_df_fine, iid_dt_fine, icc_df, icc_dt; a dt book needs the
  recovered df book of the same kind.
"""
import itertools
import json
import os
import sys
from fractions import Fraction

import numpy as np

import psprobe
import pssyntax as S


class Context:
    def __init__(self, book, df):
        self.book = book
        self.kind, self.coding = book.split('_')[:2]
        fine = book.endswith('fine')
        self.nb = 20
        if self.kind == 'iid':
            self.hdr = S.header(iid=4 if fine else 1)
        else:
            self.hdr = S.header(icc=1)
        self.df = df
        self.zero = None

    def frames(self, test, base=0, known=None):
        """Payload frames for codeword bits `test` in the last band, the other bands at
        index `base` (df: by `known` codewords of this book; dt: by a lead df frame)."""
        tail = '0' * 64
        n = self.nb - 1
        if self.coding == 'df':
            pre = compose(known, base, n) if base else []
            pre += [self.zero] * (n - len(pre))
            return [self.hdr + S.grid(0, 1) + '0' + ''.join(pre) + test + tail], \
                len(self.hdr) + 4 + len(''.join(pre))
        lead = compose(self.df['known'], base, self.nb) if base else []
        lead += [self.df['zero']] * (self.nb - len(lead))
        pre = [self.zero] * n
        return [('normal', self.hdr + S.grid(0, 1) + '0' + ''.join(lead)),
                self.hdr + S.grid(0, 1) + '1' + ''.join(pre) + test + tail], \
            len(self.hdr) + 4 + len(''.join(pre))

    def hold(self, test, base, known):
        """The frames of `frames` plus one that keeps the values (for measurement)."""
        fr, _ = self.frames(test, base, known)
        fr = [f[1] if isinstance(f, tuple) else f for f in fr]
        if self.coding == 'df':
            return fr + [fr[-1]]
        return fr + [self.hdr + S.grid(0, 1) + '1' + self.zero * self.nb + '0' * 64]


def compose(known, target, n):
    """At most n known codewords whose values sum to target (greedy)."""
    seq, left = [], target
    vals = sorted(known, key=lambda v: -abs(v))
    while left:
        v = next(v for v in vals if v != 0 and abs(v) <= abs(left) and (v > 0) == (left > 0))
        seq.append(known[v])
        left -= v
    assert len(seq) <= n, (target, seq)
    return seq


def value_of(ctx, res):
    db, c = res
    if ctx.kind == 'iid':
        return float(np.median(db[37:62]))
    return float(np.median(c[37:62].real))


def find_zero(ctx, work, maxlen=8):
    cands = [''.join(b) for n in range(1, maxlen + 1) for b in itertools.product('01', repeat=n)]
    batch, meta = [], []
    for s in cands:
        if ctx.coding == 'df':
            fr = [ctx.hdr + S.grid(0, 1) + '0' + s * ctx.nb + '0' * 64]
        else:
            fr = [('normal', ctx.hdr + S.grid(0, 1) + '0' + ctx.df['zero'] * ctx.nb),
                  ctx.hdr + S.grid(0, 1) + '1' + s * ctx.nb + '0' * 64]
        base = len(ctx.hdr) + 4
        meta.append((s, base, len(batch) + len(fr) - 1))
        batch += fr
    got = psprobe.consumed(os.path.join(work, ctx.book + '-zero.aac'), batch)
    codes = [s for s, base, at in meta if got[at] == base + ctx.nb * len(s)]
    # a repeated codeword also matches its own repetitions: keep the shortest period
    codes = [s for s in codes if not any(len(s) % len(t) == 0 and t * (len(s) // len(t)) == s
                                         for t in codes if len(t) < len(s))]
    # the zero codeword leaves the levels at index 0 in every frame it codes (a dt
    # codeword of another value moves them, or makes a frame the reference drops)
    frames, groups = [], []
    for s in codes:
        start = len(frames)
        if ctx.coding == 'df':
            frames += [ctx.hdr + S.grid(0, 1) + '0' + s * ctx.nb + '0' * 64] * 2
        else:
            frames += [ctx.hdr + S.grid(0, 1) + '0' + ctx.df['zero'] * ctx.nb + '0' * 64]
            frames += [ctx.hdr + S.grid(0, 1) + '1' + s * ctx.nb + '0' * 64] * 3
        groups.append(range(start + 1, len(frames)))
    at = [i for g in groups for i in g]
    res = dict(zip(at, psprobe.measure_frames(os.path.join(work, ctx.book + '-zero-m.aac'), frames, at)))
    ref = 0.0 if ctx.kind == 'iid' else 1.0
    vals = [[value_of(ctx, res[i]) for i in g] for g in groups]
    print('repeatable codewords', {c: [round(x, 3) for x in v] for c, v in zip(codes, vals)})
    zero = [s for s, v in zip(codes, vals) if all(abs(x - ref) < 0.02 for x in v)]
    assert len(zero) == 1, zero
    return zero[0]


def enumerate_book(ctx, work, start, base, known, tag, log=print):
    leaves, refused, frontier, rnd = [], [], list(start), 0
    while frontier:
        rnd += 1
        batch, meta = [], []
        for p in frontier:
            fr, pre_len = ctx.frames(p, base, known)
            meta.append((p, pre_len, len(batch) + len(fr) - 1))
            batch += fr
        got = psprobe.consumed(os.path.join(work, '%s-%s-%d.aac' % (ctx.book, tag, rnd)), batch)
        nxt = []
        for p, pre_len, at in meta:
            m = got[at]
            if not isinstance(m, int):
                refused.append(p)
                log('  refused %s at base %d: %s' % (p, base, m))
                continue
            L = m - pre_len
            assert len(p) <= L <= 32, (p, m, L)
            leaf = p + '0' * (L - len(p))
            leaves.append(leaf)
            nxt += [leaf[:d] + '1' for d in range(len(p), L)]
        log('%s round %d: %d leaves, %d next, %d refused' % (tag, rnd, len(leaves), len(nxt), len(refused)))
        frontier = nxt
    return leaves, refused


def measure(ctx, work, codes, base, known, tag):
    frames, at = [], []
    for c in codes:
        frames += ctx.hold(c, base, known)
        at.append(len(frames) - 1)
    res = psprobe.measure_frames(os.path.join(work, '%s-m%s.aac' % (ctx.book, tag)), frames, at)
    return [value_of(ctx, r) for r in res]


def walk_grid(ctx, work, leaves, meas):
    """The parameter grid: from index 0, step through the indices with the codewords of
    the smallest positive and negative change (unambiguous from index 0), one more band
    each time, until the reference refuses the index."""
    ref = 0.0 if ctx.kind == 'iid' else 1.0
    steps = []
    for sign in (1, -1):
        cand = [(abs(m - ref), c) for c, m in zip(leaves, meas)
                if abs(m - ref) > 0.02 and ((m > ref) == (sign > 0)) == (ctx.kind == 'iid')]
        if ctx.kind == 'icc' and sign < 0:
            cand = [(abs(m - ref), c) for c, m in zip(leaves, meas) if m - ref > 0.02]
        if cand:
            steps.append((sign, min(cand)[1]))
    grid = {0: ref}
    for sign, code in steps:
        frames, at, ks = [], [], []
        for k in range(1, ctx.nb):
            body = ctx.hdr + S.grid(0, 1) + '0' + code * k + ctx.zero * (ctx.nb - k) + '0' * 64
            frames += [body, body]
            at.append(len(frames) - 1)
            ks.append(k)
        got = psprobe.consumed(os.path.join(work, ctx.book + '-walk-l%d.aac' % sign), frames)
        res = psprobe.measure_frames(os.path.join(work, ctx.book + '-walk%d.aac' % sign), frames, at)
        for k, a, r in zip(ks, at, res):
            if not isinstance(got[a], int):
                break
            grid[sign * k] = value_of(ctx, r)
    return grid, {sign: code for sign, code in steps}


def merge_siblings(nodes):
    nodes = set(nodes)
    changed = True
    while changed:
        changed = False
        for p in sorted(nodes, key=len, reverse=True):
            q = p[:-1] + ('1' if p[-1] == '0' else '0')
            if p and q in nodes:
                nodes -= {p, q}
                nodes.add(p[:-1])
                changed = True
                break
    return sorted(nodes)


def values_by_range(ctx, work, codes, lo, hi, known):
    """Each codeword's value from the bases after which the reference accepts it: a value
    v >= 0 is accepted up to base hi - v, a value v < 0 down to base lo - v."""
    batch, meta = [], []
    for c in codes:
        for base in range(lo, hi + 1):
            fr, _ = ctx.frames(c, base, known)
            meta.append((c, base, len(batch) + len(fr) - 1))
            batch += fr
    got = psprobe.consumed(os.path.join(work, ctx.book + '-range.aac'), batch)
    ok = {}
    for c, base, at in meta:
        ok.setdefault(c, []).append((base, isinstance(got[at], int)))
    values = {}
    for c, rows in ok.items():
        legal = [b for b, g in rows if g]
        assert legal, c
        a, b = min(legal), max(legal)
        assert legal == list(range(a, b + 1)), (c, rows)
        if b < hi:
            v = hi - b
            assert a == lo, (c, rows)
        elif a > lo:
            v = lo - a
        else:
            v = 0
        values[c] = v
    return values


def main():
    book, out, work = sys.argv[1], sys.argv[2], sys.argv[3]
    df = json.load(open(sys.argv[4])) if len(sys.argv) > 4 else None
    if df:
        df['known'] = {int(k): c for k, c in df['known'].items()}
    os.makedirs(work, exist_ok=True)
    ctx = Context(book, df)
    ctx.zero = find_zero(ctx, work)
    print('zero codeword', ctx.zero)
    leaves, refused = enumerate_book(ctx, work, [''], 0, None, 'b0')
    meas = measure(ctx, work, leaves, 0, None, 'b0')
    if ctx.coding == 'df':
        grid, steps = walk_grid(ctx, work, leaves, meas)
        known = {0: ctx.zero, **steps}
    else:
        grid = {int(k): v for k, v in df['levels'].items()}
        known = None
    print('levels', {k: round(v, 4) for k, v in sorted(grid.items())})
    lo, hi = min(grid), max(grid)
    def sweep(nodes, tag):
        found = []
        for base in [hi, lo] * 4 + list(range(lo, hi + 1)):
            if not nodes:
                break
            lv, nodes = enumerate_book(ctx, work, nodes, base, known, '%s%d' % (tag, base))
            found += lv
        return found, nodes

    lv, refused = sweep(refused, 'b')
    leaves += lv
    # a node refused at every base may still hold accepted leaves beside a refused one:
    # split it, up to four levels below where it was first refused
    for level in range(4):
        if not refused:
            break
        kids = [p + b for p in refused for b in '01']
        lv, still = sweep(kids, 's%d-' % level)
        leaves += lv
        refused = merge_siblings(still)
    codes = sorted(set(leaves), key=lambda c: (len(c), c))
    k = sum(Fraction(1, 2 ** len(c)) for c in codes)
    print('codewords %d, kraft %s, refused %s' % (len(codes), k, refused))
    # nodes refused after every base: the reference rejects any codeword below them
    refused = sorted(set(refused))
    if refused:
        k += sum(Fraction(1, 2 ** len(p)) for p in refused)
        print('refused at every base:', refused, 'kraft with them', k)
    values = values_by_range(ctx, work, codes, lo, hi, known)
    # cross-check: the levels measured from index 0
    for c, m in zip(leaves, meas):
        near = min(grid, key=lambda i: abs(grid[i] - m))
        assert near == values[c], (c, values[c], m, grid[values[c]])
    vals = sorted(values.values())
    print('values', vals)
    json.dump({'book': book, 'zero': ctx.zero,
               'codewords': {c: values[c] for c in codes},
               'known': {str(v): c for c, v in values.items()},
               'levels': {str(k): v for k, v in sorted(grid.items())},
               'refused': refused,
               'unobserved_values': sorted(set(range(lo - hi, hi - lo + 1)) - set(vals))},
              open(out, 'w'), indent=1)
    full = list(range(lo - hi, hi - lo + 1))
    assert k == 1 and len(set(vals)) == len(vals) and set(vals) <= set(full), 'incomplete book'
    assert len(vals) + len(refused) == len(full), 'unexplained codewords'


if __name__ == '__main__':
    main()
