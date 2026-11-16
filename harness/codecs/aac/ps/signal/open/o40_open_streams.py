"""o40: the crafted streams of four further open cases, each the smallest stream found that
shows its difference between this decoder and the reference.

  noext-after-error
                  IPD/OPD with headers that disable the PS extension, mixed with IID/ICC
                  mode changes, one layout (o22 set R, seed 306, NENVS=1,
                  EXTS=ipd,ipd,off,none, PSW=0), reduced frame by frame: what is left is a
                  frame with IPD/OPD, an error frame, a header without the extension (o41)
  dt-envelope-reference
                  o32's seed 9818 (verify_compose's generator with frames without the SBR
                  element, no layout change), reduced frame by frame while the composed
                  model misses the reference as well
  sbr-after-missing
                  HE-AAC v1: a FIXVAR frame whose envelope ends 3 time slots into the next
                  frame, that frame without its SBR element, then frames with SBR
  sbr-truncated   neutral PS; one frame's SBR payload cut to its first 30 bits

The reduction removes one frame at a time and keeps the removal when the stream still
shows the difference (some frame above LIMIT, the criterion of each case), until no single
removal does. Writes OUTDIR/ps-open-<name>.adts and prints each stream's per-frame
residuals.

usage: o40_open_streams.py OUTDIR [decode_aac]"""
import o31_no_sbr_kinds as O31

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
BIN = sys.argv[2] if len(sys.argv) > 2 else DECODE
LIMIT = 1e-6


def residuals(spec, seed, model=None):
    """Per-frame rms (both channels, worst) of this decoder and, with frame dicts, of the
    composed model (olib.PersistComposer) against the reference."""
    p = os.path.join(out, 'probe.aac')
    build2(p, spec, seed=seed)
    y, msgs, _ = decode_log(p)
    z = rust(p, BIN)
    rz = per_frame(z, y)[:-1].max(1)
    rm = None
    if model is not None:
        X, _ = dump(p)
        comp = PersistComposer({'form': 'lat'})
        L, R = comp.run(X, model)
        rm = per_frame(model_out(L, R, len(y)), y)[:-1].max(1)
    return rz, rm, msgs


def reduce(spec, seed, shows, model=None):
    """Returns the reduced spec and model and the original indices of the frames kept."""
    spec, model = list(spec), None if model is None else list(model)
    keep = list(range(len(spec)))
    changed = True
    while changed:
        changed = False
        for i in range(len(spec) - 1, -1, -1):
            s2 = spec[:i] + spec[i + 1:]
            m2 = None if model is None else model[:i] + model[i + 1:]
            if len(s2) < 3:
                continue
            rz, rm, _ = residuals(s2, seed, m2)
            if shows(rz, rm):
                spec, model, changed = s2, m2, True
                keep = keep[:i] + keep[i + 1:]
    print('   kept frames (original indices): %s' % keep)
    return spec, model


def save(name, spec, seed, model=None):
    p = os.path.join(out, 'ps-open-%s.adts' % name)
    build2(p, spec, seed=seed)
    rz, rm, msgs = residuals(spec, seed, model)
    print('== %s: %d frames, %d bytes' % (name, len(spec), os.path.getsize(p)))
    print('   decoder - reference per frame: %s' % fmt(rz))
    if rm is not None:
        print('   model - reference per frame:   %s' % fmt(rm))
    print('   messages: %s' % sorted(set(msgs)))


if __name__ == '__main__':
    # (a) IPD/OPD, headers without the extension, mode changes, one layout
    os.environ['NENVS'], os.environ['EXTS'] = '1', 'ipd,ipd,off,none'
    import o22_ipd_random as O22
    O22.NENVS, O22.EXTS = [1], ['ipd', 'ipd', 'off', 'none']
    spec = O22.make(np.random.default_rng(306), False, p_switch=0.0)
    print('\n'.join('   original ' + l for l in O22.LOG))
    spec, _ = reduce(spec, 306, lambda rz, rm: rz.max() > LIMIT)
    save('noext-after-error', spec, 306)

    # (b) verify_compose seed 9818, frames without the SBR element, no layout change
    VC.NOSBR = True
    spec, model = VC.make_stream(np.random.default_rng(9818), p_switch=0.0, start_cfg=20, allow_change=True)
    for i, m in enumerate(model):
        print('   original f%d %s' % (i, {k: (v if k not in ('iid', 'icc') or v is None else
                                            [(dt, len(d)) for dt, d in v]) for k, v in m.items()}))
    spec, model = reduce(spec, 9818, lambda rz, rm: rz.max() > LIMIT and rm.max() > LIMIT, model)
    save('dt-envelope-reference', spec, 9818, model)

    # (c) HE-AAC v1: the frame after a missing SBR element that a FIXVAR envelope reached into
    spec = [None, ('sbrhook', None, O31.fixvar), ('nosbr',), None, None]
    save('sbr-after-missing', spec, 11)

    # (d) neutral PS, one SBR payload cut to 30 bits
    par = O31.KINDS['neutral']
    spec = [par, ('sbrhook', par, lambda b: b[:30]), par, par]
    save('sbr-truncated', spec, 11)
