"""o31 (case 3): which PS streams show a difference in a frame without its SBR element.
Streams of 9 frames, frame 4 without the SBR element, with: neutral PS; ICC index 7 (L = D,
R = -D); IID +7 (L and R scaled apart); ICC 4; a frame without PS before the missing one;
frames whose SBR data is cut short (30 or 60 bits) instead of missing; and a frame before
it whose SBR envelope reaches into it (FIXVAR, also without PS). This decoder against the
reference, per frame.

usage: o31_no_sbr_kinds.py OUTDIR"""
from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
NOSBR = 4
KINDS = {
    'neutral': pdata(iid=[[0] * 20], icc=[[0] * 20]),
    'icc7': pdata(icc=[[7] * 20]),
    'icc4': pdata(icc=[[4] * 20]),
    'iid+7': pdata(iid=[[7] * 20]),
    'iid-3': pdata(iid=[[-3] * 20]),
}


def fixvar(bits):
    """SBR payload bits with psgen's FIXFIX grid (bits 18..22: class 0, one envelope, low
    resolution) replaced by FIXVAR: var_bord_1 3, one envelope, pointer 0, low resolution."""
    assert bits[18:23] == [0, 0, 0, 0, 0]
    return bits[:18] + [0, 1, 1, 1, 0, 0, 0, 0] + bits[23:]


def run(name, spec, seed=11):
    p = os.path.join(out, 'o31_%s_%d.aac' % (name, seed))
    build2(p, spec, seed=seed)
    y, msgs, _ = decode_log(p)
    z = rust(p)
    r = per_frame(z, y).max(1)
    print('  %-22s seed %d: %s' % (name, seed, fmt(r)))
    return p, y, z


if __name__ == '__main__':
    for seed in (11, 12):
        for name, par in KINDS.items():
            spec = [par] * 9
            spec[NOSBR] = ('nosbr',)
            run(name, spec, seed)
            spec[NOSBR - 1] = None          # a frame without PS before it
            run(name + '+nops', spec, seed)
            for cut in (30, 60):            # SBR data cut short (the PS data is lost too)
                spec = [par] * 9
                spec[NOSBR] = ('sbrhook', par, lambda b, c=cut: b[:c])
                run('%s+trunc%d' % (name, cut), spec, seed)
            # the frame before has a FIXVAR grid whose envelope ends 3 time slots (6 QMF
            # slots) into the frame without SBR: its high band reaches into that frame
            spec = [par] * 9
            spec[NOSBR - 1] = ('sbrhook', par, fixvar)
            spec[NOSBR] = ('nosbr',)
            run(name + '+fixvar', spec, seed)
    # the same without PS (HE-AAC v1)
    for seed in (11, 12):
        spec = [None] * 9
        spec[NOSBR - 1] = ('sbrhook', None, fixvar)
        spec[NOSBR] = ('nosbr',)
        p, y, z = run('v1+fixvar', spec, seed)
