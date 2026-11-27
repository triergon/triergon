"""o42: what an error frame leaves of the IPD/OPD values. The noext shape of o41 (a header
frame with IPD/OPD, an error frame, a header frame), with the third frame's extension
coding IPD/OPD time-differentially: against indices 0, or against the first frame's
values. Each is compared with the same values coded frequency-differentially; the one
that matches is the reference the third frame reads. A control without the error frame
checks that a time-differential envelope reads the previous frame's values there.

usage: o42_error_phase_reference.py OUTDIR"""
from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
rng = np.random.default_rng(42)


def walk(n, lo, hi):
    return [int(v) for v in np.clip(int(rng.integers(lo, hi + 1)) + np.cumsum(rng.integers(-2, 3, n)), lo, hi)]


def ipd_dt(idx, prev, book):
    return ''.join(B[book][(v - p) % 8] for v, p in zip(idx, prev))


def ext(ipd, opd, prev=None):
    """IPD/OPD extension body: df coded, or dt coded against `prev` (ipd, opd)."""
    if prev is None:
        return ipdopd_ext(ipd, opd)
    return '00' + '1' + '1' + ipd_dt(ipd, prev[0], 'IPD_DT') + '1' + ipd_dt(opd, prev[1], 'OPD_DT') + '0'


first = ([int(v) for v in rng.integers(0, 8, 17)], [int(v) for v in rng.integers(0, 8, 17)])
third = ([int(v) for v in rng.integers(0, 8, 17)], [int(v) for v in rng.integers(0, 8, 17)])
iid34a, iid34b = walk(34, -15, 15), walk(34, -7, 7)
icc34a, icc34b = walk(34, 0, 7), walk(34, 0, 7)
f_a = payload(iid=iid34a, iid_mode=5, icc=icc34a, icc_mode=5, ext=ext(*first))
f_err = payload(iid=iid34a, iid_mode=5, icc=[7, 8] + [8] * 32, icc_mode=5, ext=ext(*first))


def f_c(e):
    return payload(iid=iid34b, iid_mode=2, icc=icc34b, icc_mode=5, ext=e)


def ref(spec):
    p = os.path.join(out, 'o42.aac')
    build2(p, spec, seed=42)
    return decode_log(p)[0]


df = ref([f_a, f_err, f_c(ext(*third)), None, None])
print('error frame, dt against indices 0 vs df:        %s' % fmt(per_frame(df, ref([f_a, f_err, f_c(ext(*third, prev=([0] * 17, [0] * 17))), None, None]))[:-1].max(1)))
print('error frame, dt against the first frame vs df:  %s' % fmt(per_frame(df, ref([f_a, f_err, f_c(ext(*third, prev=first)), None, None]))[:-1].max(1)))
df2 = ref([f_a, f_c(ext(*third)), None, None])
print('no error frame, dt against the first frame vs df: %s' % fmt(per_frame(df2, ref([f_a, f_c(ext(*third, prev=first)), None, None]))[:-1].max(1)))
