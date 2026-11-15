"""o05 (case 1): o01's ro and len streams against the composed model whose decorrelator
state is carried by band index (and the ducker's by parameter-band index) at a change
registered while off, for each realization of the all-pass sections (FormDecorrelator).
Prints the frame residuals and a per-QMF-band look at the corner frame.

usage: o05_carry_forms.py OUTDIR"""
import o01_change_off as O1

from olib import *

out = sys.argv[1]
src = os.path.join(os.path.dirname(out.rstrip('/')), 'o01')


def look(y, L, corner):
    d = y[:, 0] - psmodel.synthesis(L)[:len(y)]
    Dq = psana.qmf_bands(np.concatenate([np.zeros(640), d]))
    s = slice(32 * corner + 10, 32 * corner + 42)
    return np.sqrt((np.abs(Dq[s]) ** 2).mean(0)) / 32768


VARIANTS = [('df1', {'form': 'df1', 'carry': 'index', 'duck': 'index'}),
            ('df2', {'form': 'df2', 'carry': 'index', 'duck': 'index'}),
            ('df2q', {'form': 'df2q', 'carry': 'index', 'duck': 'index'}),
            ('tdf2', {'form': 'tdf2', 'carry': 'index', 'duck': 'index'}),
            ('apzero', {'carry': 'index', 'duck': 'index', 'zero': 'ap'}),
            ('lowap0', {'carry': 'index', 'duck': 'index', 'zero': 'lowap'}),
            ('hin0', {'carry': 'index', 'duck': 'index', 'zero': 'hin'})]
VARIANTS += [(f + '/p', {'form': f, 'carry': 'index', 'duck': 'index', 'zero': 'oldplain'})
             for f in ('df1', 'df2', 'df2q', 'tdf2', 'lat', 'latq')]
VARIANTS += [('lat/p+' + z, {'form': 'lat', 'carry': 'index', 'duck': 'index', 'zero': 'oldplain+' + z})
             for z in ('lowhin', 'lowap', 'low')]
if len(sys.argv) > 2:
    VARIANTS = [v for v in VARIANTS if v[0] in sys.argv[2].split(',')]

if __name__ == '__main__':
    for old, new in ((34, 20), (20, 34)):
        for kind, corner in (('ro', 6), ('len', 5)):
            frames = O1.stream(kind, old, new)
            p = os.path.join(src, 'o01_%s_%d_%d.aac' % (kind, old, new))
            model = [f[1] for f in frames]
            y, msgs, _ = decode_log(p)
            X, _ = dump(p)
            for name, pol in VARIANTS:
                comp = CarryComposer(pol)
                L, R = comp.run(X, model)
                r = per_frame(model_out(L, R, len(y)), y).max(1)
                eb = look(y, L, corner)
                print('== %s %d -> %d %-5s frames %s' % (kind, old, new, name, fmt(r[corner - 1:corner + 4])))
                for k0 in range(0, 32, 16):
                    print('   %2d: %s' % (k0, fmt(eb[k0:k0 + 16])))
