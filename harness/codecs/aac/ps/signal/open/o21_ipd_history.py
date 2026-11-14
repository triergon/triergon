"""o21 (case 2): what happens to the IPD/OPD smoothing history at a 20 <-> 34 change.
Crafted streams, one envelope per frame, IPD/OPD in every frame:
  iid34_20 / iid20_34   IID and ICC enabled in both layouts (11 <-> 17 IPD bands)
  open_like             34 bands, then IID disabled and ICC mode 1 (17 coded IPD bands)
  open_icc0             the same with ICC index 0 everywhere (no decorrelated part)
  stay34                34 bands, then IID disabled and ICC mode 2: no change of layout
  repeat                a change frame followed by a frame without PS (which repeats it)
  corner34_20 / corner20_34
                        a change registered while off (error frame, header-less frame),
                        ICC index 0 so that the decorrelator does not enter
Each stream is decoded by the reference and by the builds given (rust_variant.py's
scratch copy, switched by PSV); per-frame residuals are printed.

usage: o21_ipd_history.py OUTDIR binary [binary ...]"""
import o20_ipd_change as O20

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)

A = ([1, 2, 3, 4, 5, 6, 7, 1, 2, 3, 4, 5, 6, 7, 1, 2, 3], [2, 1, 0, 7, 6, 5, 4, 3, 2, 1, 0, 7, 6, 5, 4, 3, 2])
B = ([5, 3, 1, 7, 2, 6, 0, 4, 3, 5, 7, 1, 6, 2, 4, 0, 3], [6, 0, 3, 5, 1, 4, 7, 2, 5, 3, 6, 0, 2, 7, 1, 4, 6])


def ext(v, nb):
    return ipdopd_ext(v[0][:nb], v[1][:nb])


def f(iid_mode=None, icc_mode=None, iid=None, icc=None, ipd=A, header=True, bad=False):
    nb = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}
    # the IPD band count follows the last IID mode; the caller passes it via ipd[2]
    nipd = ipd[2] if len(ipd) > 2 else 17
    e = ext(ipd, nipd)
    if iid_mode is not None and iid is None:
        iid = [((b * 5) % 7) - 3 for b in range(nb[iid_mode])]
    if icc_mode is not None and icc is None:
        icc = [0] * nb[icc_mode]
    if bad:
        icc = [7, 8] + [8] * (nb[icc_mode] - 2)
    if header:
        return payload(iid=iid, iid_mode=iid_mode if iid_mode is not None else 1, icc=icc,
                       icc_mode=icc_mode if icc_mode is not None else 1, ext=e)
    return noheader(iid=iid, iid_mode=iid_mode if iid_mode is not None else 1, icc=icc,
                    icc_mode=icc_mode if icc_mode is not None else 1, ext=e)


def streams():
    A17, B17, B11, A11 = A + (17,), B + (17,), B + (11,), A + (11,)
    rnd = np.random.default_rng(5)
    icc34 = [int(v) for v in rnd.integers(0, 8, 34)]
    icc20 = [int(v) for v in rnd.integers(0, 8, 20)]
    s = {}
    s['iid34_20'] = [f(2, 2, ipd=A17, icc=icc34)] * 4 + [f(1, 1, ipd=B11, icc=icc20)] * 6
    s['iid20_34'] = [f(1, 1, ipd=A11, icc=icc20)] * 4 + [f(2, 2, ipd=B17, icc=icc34)] * 6
    s['open_like'] = [f(2, 2, ipd=A17, icc=icc34)] * 4 + [f(None, 1, ipd=B17, icc=icc20)] * 6
    s['open_icc0'] = [f(2, 2, ipd=A17)] * 4 + [f(None, 1, ipd=B17)] * 6
    s['stay34'] = [f(2, 2, ipd=A17, icc=icc34)] * 4 + [f(None, 2, ipd=B17, icc=icc34)] * 6
    s['repeat'] = [f(2, 2, ipd=A17, icc=icc34)] * 4 + [f(None, 1, ipd=B17, icc=icc20), None] + \
        [f(None, 1, ipd=B17, icc=icc20)] * 4
    s['corner34_20'] = [f(2, 2, ipd=A17)] * 4 + [f(None, 1, ipd=B17, bad=True),
                                                 f(None, 1, ipd=B17, header=False)] + [f(None, 1, ipd=B17)] * 5
    s['corner20_34'] = [f(1, 1, ipd=A11)] * 4 + [f(None, 2, ipd=B11, bad=True),
                                                 f(None, 2, ipd=B11, header=False)] + [f(None, 2, ipd=B11)] * 5
    C17 = ([3, 6, 1, 4, 7, 2, 5, 0, 6, 3, 1, 7, 4, 2, 5, 3, 1], [7, 4, 1, 6, 3, 0, 5, 2, 7, 4, 1, 6, 3, 0, 5, 2, 7], 17)
    C11 = C17[:2] + (11,)
    # two corners in a row: bands one layout leaves alone come back
    s['corners34_20_34'] = [f(2, 2, ipd=A17)] * 4 + [f(None, 1, ipd=B17, bad=True), f(None, 1, ipd=B17, header=False)] + \
        [f(None, 1, ipd=B17)] * 3 + [f(None, 2, ipd=C17, bad=True), f(None, 2, ipd=C17, header=False)] + \
        [f(None, 2, ipd=C17)] * 4
    s['corners20_34_20'] = [f(1, 1, ipd=A11)] * 4 + [f(None, 2, ipd=B11, bad=True), f(None, 2, ipd=B11, header=False)] + \
        [f(None, 2, ipd=B11)] * 3 + [f(None, 1, ipd=C11, bad=True), f(None, 1, ipd=C11, header=False)] + \
        [f(None, 1, ipd=C11)] * 4
    return s


if __name__ == '__main__':
    bins = sys.argv[2:]
    only = os.environ.get('ONLY', '').split(',') if os.environ.get('ONLY') else None
    for name, pays in streams().items():
        if only and name not in only:
            continue
        p = os.path.join(out, 'o21_%s.aac' % name)
        write(p, pays + [None, None], seed=13)
        O20.compare(p, name, bins, bands=bool(only))
