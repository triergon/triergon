"""V_MIX2 (m33): the whole mixing with the dec recovery's D (decmodel, ducking included) in every
band, so the D column is checked where IPD/OPD act. Fresh streams, 24 frames, 1/2/4
envelopes, random IID and ICC indices in every band, random IPD/OPD, frames with the
extension, without it (header enable_ext 0), with the enable bit 0, and header-less frames
(enable_ps_header 0; they use the last header's flags, so they carry an extension exactly
when that header said so). Compared from frame 2 on with the model and with variants:
  d_phase conj   the D column turned by the opposite phase
  image M only   the image rule (conjugate start) on h11, h21 only
  image off      no image rule
usage: v_mix2.py OUT [variants]"""
import sys
from mixlib import *
import mixmodel as MM

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'hyb'))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'dec'))
import hybmodel as HM  # noqa: E402
import decmodel as DM  # noqa: E402

NF = 24


def ext_env(ipds, opds):
    s = '00' + '1'
    for i, o in zip(ipds, opds):
        s += '0' + ipd_df(i, 'IPD_DF') + '0' + ipd_df(o, 'OPD_DF')
    return s + '0'


def make(seed, iid_mode, icc_mode, kinds=('ipd', 'ipd', 'none', 'off', 'nohdr', 'nohdr'), icc_max=8):
    rng = np.random.default_rng(seed)
    nmax = 15 if iid_mode >= 3 else 7
    ni, nc = NB[iid_mode], NB[icc_mode]
    nip = MM.NIPD[ni]
    pays, frames = [], []
    hdr_ext = True
    for f in range(NF):
        nenv = int(rng.choice([1, 2, 4]))
        iid = [[int(v) for v in rng.integers(-nmax, nmax + 1, ni)] for _ in range(nenv)]
        icc = [[int(v) for v in rng.integers(0, icc_max, nc)] for _ in range(nenv)]
        ipd = [[int(v) for v in rng.integers(0, 8, nip)] for _ in range(nenv)]
        opd = [[int(v) for v in rng.integers(0, 8, nip)] for _ in range(nenv)]
        kind = str(rng.choice(list(kinds))) if f > 2 else 'ipd'
        header = kind != 'nohdr'
        if not header:
            kind = str(rng.choice(['ipd', 'off'])) if hdr_ext else 'none'
        while True:
            ext = {'ipd': ext_env(ipd, opd), 'off': '00' + '0' + '0', 'none': None}[kind]
            if header:
                p = payload(iid=iid, icc=icc, iid_mode=iid_mode, icc_mode=icc_mode, nenv=nenv, ext=ext)
            else:
                # enable_ps_header 0 (pslib's payload(header=False) leaves the bit out); the
                # extension size field follows the data when the last header enabled it
                p = '0' + payload(iid=iid, icc=icc, iid_mode=iid_mode, icc_mode=icc_mode, nenv=nenv,
                                  ext=ext, header=False)
            if len(p) <= 1900 or nenv == 1:   # the fill element holds at most 269 bytes
                break
            nenv //= 2
            iid, icc, ipd, opd = iid[:nenv], icc[:nenv], ipd[:nenv], opd[:nenv]
        if header:
            hdr_ext = ext is not None
        pays.append(p)
        fr = {'iid': iid, 'icc': icc, 'nenv': nenv, 'ipd': ipd, 'opd': opd,
              'ext': {'ipd': 'ipd', 'off': 'off', 'none': None}[kind], 'header': header}
        if header:
            fr['iid_mode'], fr['icc_mode'] = iid_mode, icc_mode
        frames.append(fr)
    return pays, frames


def check(name, seed, iid_mode, icc_mode, cfg, outdir, variants=(('model', {}),), **kw):
    pays, frames = make(seed, iid_mode, icc_mode, **kw)
    p = os.path.join(outdir, 'v2.aac')
    write(p, pays, seed=seed)
    y = decode(p)
    X, _ = dump(p)
    n = min(len(X), len(y) // 64, 32 * NF)
    X = X[:n]
    D = DM.Decorrelator(cfg).process(DM.analysis(X, cfg))
    plan = ' '.join('%d:%d%s%s' % (i, f['nenv'], {'ipd': 'P', 'off': 'o', None: '-'}[f['ext']], '' if f['header'] else 'h')
                    for i, f in enumerate(frames))
    print('%s plan: %s; longest payload %d bits' % (name, plan, max(len(q) for q in pays)))
    out = {}
    s0, s1 = 2 * 2048, 64 * n - 640
    for vname, kwm in variants:
        mx = MM.Mixer(cfg, HM, DM=DM, **kwm)
        L, R = mx.run(X, D, frames)
        res = []
        for ch, Y in ((0, L), (1, R)):
            r = y[s0:s1, ch] - psmodel.synthesis(Y)[s0:s1]
            res.append((float(np.sqrt(np.mean(r ** 2))), float(np.abs(r).max()), float(np.sqrt(np.mean(y[s0:s1, ch] ** 2)))))
        if vname == 'model':
            r = y[:64 * n, 0] - psmodel.synthesis(L)
            print('  per frame L:', ' '.join('%.0e' % np.sqrt(np.mean(r[2048 * i:2048 * i + 2048] ** 2)) for i in range(2, n // 32)))
        print('  %-14s L rms %.3g max %.3g | R rms %.3g max %.3g | signal %.3g (%.1f dB)' % (
            vname, res[0][0], res[0][1], res[1][0], res[1][1], res[0][2], 20 * np.log10(res[0][0] / res[0][2])))
        out[vname] = res
    return out


VARIANTS = (('model', {}), ('d_phase conj', {'d_phase': 'conj'}), ('image M only', {'image_cols': (0, 2)}),
            ('image off', {'image': False}))
CASES = (('20 A', 91, 1, 1, 20), ('20 B fine', 92, 4, 4, 20), ('34 A', 93, 2, 2, 34),
         ('34 B (IID 20 fine)', 94, 4, 5, 34), ('34 A (IID 10)', 95, 0, 2, 34))

if __name__ == '__main__':
    if len(sys.argv) > 2:
        CASES = [c for c in CASES if c[0] == sys.argv[2]]
        VARIANTS = VARIANTS[:1]
    for case in CASES:
        check(*case, sys.argv[1], variants=VARIANTS)
