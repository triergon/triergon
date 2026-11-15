"""o01 (case 1): a 20 <-> 34 change registered while the PS is off, on crafted streams with
ICC index 7 in every band (L = D, R = -D: the matrices are the same in both layouts, so
only the decorrelator, ducking and hybrid state can differ). Streams, for 34 -> 20 and
20 -> 34:
  ro      4 frames old layout; an error frame with a header of the new layout (illegal
          ICC); a header-less frame read while off (registers the change); frames that run
  len     4 frames old layout; a new-layout frame refused for its size (registers it);
          frames that run
  err     4 frames old layout; an error frame with a new-layout header; frames that run
          (the change registers at the frame that runs: the known rule)
  direct  4 frames old layout; new-layout frames that run (the known rule)
Each stream is compared with the composed model under several carry-over policies.

usage: o01_change_off.py OUTDIR"""
from olib import *

MODE = {20: 1, 34: 2}


def stream(kind, old, new, nrun=5):
    nb = {20: 20, 34: 34}
    mo, mn = MODE[old], MODE[new]
    ok_old = pf(icc=[7] * nb[old], icc_mode=mo)
    ok_new = pf(icc=[7] * nb[new], icc_mode=mn)
    bad_new = pf(icc=[7, 8] + [8] * (nb[new] - 2), icc_mode=mn)
    fr = [ok_old] * 4
    if kind == 'ro':
        fr += [bad_new, pf(icc=[7] * nb[new], header=False, modes=(None, mn))]
    elif kind == 'len':
        s, d = ok_new
        fr += [(('declared', s, 1), dict(d, error='length'))]
    elif kind == 'err':
        fr += [bad_new]
    fr += [ok_new] * nrun + [nops(), nops()]
    return fr


POLICIES = [('reset', {}), ('index', {'dec': 'index'}), ('qmf', {'dec': 'qmf'}),
            ('qmfall', {'dec': 'qmfall'}), ('separate', {'dec': 'separate'}),
            ('lowidx', {'dec': 'lowidx'}), ('lowidx+qmf', {'dec': 'lowidx+qmf'}),
            ('lowfreq', {'dec': 'lowfreq'}), ('reset,duck-index', {'duck': 'index'}),
            ('index,duck-index', {'dec': 'index', 'duck': 'index'}),
            ('reset,hist0', {'hist': 'zero'}), ('stay', {'corner': 'stay'})]


def main(out):
    for old, new in ((34, 20), (20, 34)):
        for kind in ('direct', 'err', 'ro', 'len'):
            frames = stream(kind, old, new)
            p = os.path.join(out, 'o01_%s_%d_%d.aac' % (kind, old, new))
            model = build_frames(p, frames, seed=11)
            y, msgs, _ = decode_log(p)
            X, _ = dump(p)
            n = len(X)
            print('== %s %d -> %d   msgs: %s' % (kind, old, new, '; '.join(msgs)))
            for name, pol in POLICIES:
                comp = Composer(pol)
                L, R = comp.run(X, model)
                m = model_out(L, R, len(y))
                r = per_frame(m, y).max(1)
                kinds = ''.join((o.reason[0] if o.reason else '?') + ('c' if o.changed else '') for o in comp.outs)
                print('  %-18s %s  | %s' % (name, fmt(r[3:12]), kinds if name == 'reset' else ''))


if __name__ == '__main__':
    os.makedirs(sys.argv[1], exist_ok=True)
    main(sys.argv[1])
