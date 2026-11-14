"""o00: the open streams of cases 1 and 3 rebuilt from their generators and compared per
frame: reference against this decoder and against the composed model.

usage: o00_repro.py OUTDIR"""
import hashlib

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
VECTORS = os.path.join(ROOT, 'harness', 'codecs', 'aac', 'ps', 'vectors')


def sha(p):
    return hashlib.sha256(open(p, 'rb').read()).hexdigest()


for name, label, seed, psw, sc, nosbr in (('ps-change-while-off.adts', 'T2', 9412, 0.15, 34, False),
                                         ('ps-no-sbr-element.adts', 'TS', 9203, 0.15, 20, True)):
    VC.NOSBR = nosbr
    rng = np.random.default_rng(seed)
    spec, model = VC.make_stream(rng, p_switch=psw, start_cfg=sc, allow_change=True)
    p = os.path.join(out, '%s_%d.aac' % (label, seed))
    build2(p, spec, seed=seed)
    print('==', name, 'rebuilt identical:', sha(p) == sha(os.path.join(VECTORS, name)))
    y, msgs, _ = decode_log(p)
    z = rust(p)
    X, _ = dump(p)
    comp = VC.C.Composer()
    L, R = comp.run(X, model)
    n = min(len(y), len(z))
    m = model_out(L, R, n)
    rz = per_frame(z, y).max(1)
    rm = per_frame(m, y).max(1)
    for f, o in enumerate(comp.outs[:len(rz)]):
        fr = model[f]
        d = {k: v for k, v in fr.items() if k not in ('iid', 'icc')}
        print('  f%-2d rust %.1e model %.1e  %-6s cfg %s chg %d  %s' % (
            f, rz[f], rm[f], o.reason, o.cfg, o.changed, d))
    for msg in msgs:
        print('   msg:', msg)
