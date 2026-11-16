"""Shared helpers of the open-case probes (o*): the reference, this decoder and the
composed model on one stream, compared per frame and per QMF band.

The decoder under test is the `decode_aac` example (DECODE_AAC, default
target/release/examples/decode_aac); the PS input comes from `ps_input` as in the other
parts."""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
for part in ('time', 'hyb', 'dec', 'mix', ''):
    p = os.path.join(HERE, '..', part)
    if p not in sys.path:
        sys.path.insert(0, p)

from timelib import *  # noqa: F401,F403,E402
import verify_compose as VC  # noqa: E402

DECODE = os.environ.get('DECODE_AAC', os.path.join(ROOT, 'target', 'release', 'examples', 'decode_aac'))


def rust(path, binary=None):
    """This decoder's output (n, 2) of an ADTS stream."""
    o = path + '.rust.f32'
    subprocess.run([binary or DECODE, path, o], capture_output=True, check=True)
    return np.fromfile(o, dtype='<f4').astype(np.float64).reshape(-1, 2)


def per_frame(a, b, nf=None):
    """(frames, 2) rms of a - b per 2048-sample frame and channel."""
    n = min(len(a), len(b))
    nf = nf or n // 2048
    return np.array([[np.sqrt(np.mean((a[2048 * f:2048 * f + 2048, c] - b[2048 * f:2048 * f + 2048, c]) ** 2))
                      for c in (0, 1)] for f in range(nf)])


def model_out(L, R, n):
    return np.stack([psmodel.synthesis(L)[:n], psmodel.synthesis(R)[:n]], 1)


def band_residual(d, f, bands=range(64)):
    """QMF slots recovered from the residual d (one channel) around frame f: the rms per
    band over the frame's 32 slots."""
    s0 = max(0, 32 * f - 12)
    seg = d[64 * s0:64 * (32 * f + 44)]
    Z, _ = recover(seg, list(bands))
    a = 32 * f - s0
    return np.sqrt(np.mean(np.abs(Z[a:a + 32]) ** 2, 0))


def fmt(v):
    return ' '.join('%.1e' % x for x in v)


# --- crafted frames: payload bits and the timeline's frame dict together ---------------

import compose as C  # noqa: E402

CFG = C.HM.config


def pf(iid=None, icc=None, iid_mode=None, icc_mode=None, header=True, nenv=1, fc=0, borders=(),
       modes=None, ext=None):
    """One PS frame: (payload bits, timeline frame dict). iid/icc: per-envelope lists of
    absolute indices (one list: every envelope), df coded; a mode of None disables the
    parameter in a header. A header-less frame is read with `modes` = (iid_mode, icc_mode)
    of the last header."""
    im, cm = (iid_mode, icc_mode) if header else modes
    envs = {}
    for name, v, m in (('iid', iid, im), ('icc', icc, cm)):
        if m is None or v is None:
            envs[name] = None
            continue
        envs[name] = v if isinstance(v[0], (list, tuple)) else [v] * max(nenv, 1)
    s = (S.header(iid=im, icc=cm, ext=ext is not None) if header else '0')
    idx = {0: 0, 1: 1, 2: 2, 4: 3}[nenv] if fc == 0 else nenv - 1
    s += S.grid(fc, idx, borders)
    fine = im is not None and im >= 3
    for name, book in (('iid', 'IID_DF_FINE' if fine else 'IID_DF'), ('icc', 'ICC_DF')):
        if envs[name] is None:
            continue
        for e in range(nenv):
            s += '0' + df_codes(envs[name][e], book)
    if ext is not None:
        n = (len(ext) + 7) // 8
        body = ext + '0' * (8 * n - len(ext))
        s += format(min(n, 15), '04b') + (format(n - 15, '08b') if n >= 15 else '') + body
    cfg = None if (im is None and cm is None) else CFG(im, cm)
    coded = {k: None if envs[k] is None else
             [(False, [int(x) for x in np.diff([0] + list(e))]) for e in envs[k][:nenv]] for k in envs}
    d = {'ps': True, 'header': {'iid_mode': iid_mode, 'icc_mode': icc_mode} if header else None,
         'frame_class': fc, 'nenv': nenv, 'borders': tuple(borders), 'cfg': cfg,
         'iid': coded['iid'], 'icc': coded['icc']}
    return s, d


def nops():
    return None, {'ps': False}


def build_frames(path, frames, seed=3):
    """frames: list of (spec entry, frame dict); writes the stream, returns the dicts. A
    spec entry is one of timelib.build2's, or a dict of psgen.Gen.frame arguments."""
    specs = [f[0] for f in frames]
    if not any(isinstance(s, dict) for s in specs):
        build2(path, specs, seed=seed)
    else:
        g = SbrGen(np.random.default_rng(seed))
        out = []
        for s in specs:
            if isinstance(s, dict):
                out.append(g.frame(**s))
            elif s is None:
                out.append(g.frame([]))
            elif isinstance(s, tuple):
                out.append(g.frame([(2, s[1])], declared=s[2]))
            else:
                out.append(g.frame([(2, s)]))
        psgen.write_stream(path, out)
    return [f[1] for f in frames]


class FormDecorrelator(C.StreamDecorrelator):
    """compose.StreamDecorrelator with the all-pass sections in another realization (the
    transfer function is the same; the state differs, which shows only when the state is
    carried between bands of different coefficients):
      df1   y = -a u + Q u[t-d] + a Q y[t-d]       (compose's; state: u and y)
      df2   w = u + a Q w[t-d], y = -a w + Q w[t-d]  (state: w)
      df2q  s = Q (u + a s[t-d]) , y = -a u ... see below (state: s = Q w)
      tdf2  y = -a u + v[t-d], v = Q u + a Q y       (state: v)
    df2q keeps s[t] = Q w[t]: w = u + a s[t-d], y = s[t-d] - a w, s = Q w... written so
    that the stored value carries the phase of the band that wrote it."""

    def __init__(self, cfg, form='df1'):
        self.form = form
        super().__init__(cfg)

    def reset(self):
        super().reset()
        nb = len(self.bands)
        self.w = [np.zeros((d, nb), complex) for d in C.DM.DELAYS]

    def process(self, H):
        out = np.zeros_like(H)
        self.glog = np.zeros((len(H), self.npar))
        ap = self.kind == 0
        DM = C.DM
        for t in range(len(H)):
            x = H[t]
            u = self.phi * self.hin[DM.PRE - 1]
            for m, d in enumerate(DM.DELAYS):
                a, q, w = self.a[m], self.q[m], self.w[m]
                old = w[d - 1]
                if self.form == 'df1':
                    y = -a * u + q * self.sx[m][d - 1] + a * q * self.sy[m][d - 1]
                    self.sx[m] = np.roll(self.sx[m], 1, 0)
                    self.sx[m][0] = u
                    self.sy[m] = np.roll(self.sy[m], 1, 0)
                    self.sy[m][0] = y
                    wt = old
                elif self.form == 'df2':
                    wt = u + a * q * old
                    y = -a * wt + q * old
                elif self.form == 'df2q':
                    # stored s = Q w: w = u + a s[t-d]; y = s[t-d] - a w; store Q w
                    wt = u + a * old
                    y = old - a * wt
                    wt = q * wt
                elif self.form == 'lat':
                    # y = Q s[t-d] - a u, s = u + a y
                    y = q * old - a * u
                    wt = u + a * y
                elif self.form == 'latq':
                    # the same with the stored value turned by Q: s' = Q (u + a y)
                    y = old - a * u
                    wt = q * (u + a * y)
                elif self.form == 'tdf2':
                    y = -a * u + old
                    wt = q * u + a * q * y
                else:
                    raise ValueError(self.form)
                self.w[m] = np.roll(w, 1, 0)
                self.w[m][0] = wt
                u = y
            D = np.where(ap, u, np.where(self.kind == 1, self.hin[DM.LONG - 1], self.hin[DM.SHORT - 1]))
            self.hin = np.roll(self.hin, 1, 0)
            self.hin[0] = x
            P = np.bincount(self.par, np.abs(x) ** 2, self.npar)
            self.pk = np.maximum(P, DM.ALPHA * self.pk)
            self.e1 = self.e1 + DM.BETA * (self.pk - P - self.e1)
            self.e2 = self.e2 + DM.BETA * (P - self.e2)
            duck = DM.GAMMA * self.e1 > self.e2
            g = np.where(duck, self.e2 / np.where(duck, DM.GAMMA * self.e1, 1.0), 1.0)
            self.glog[t] = g
            out[t] = D * g[self.par]
        return out


class Composer(C.Composer):
    """compose.Composer with more carry-over rules for the decorrelator when the layout of
    the frame that runs differs from that of the last frame that ran ('dec' policy):
      lowidx      low hybrid bands keep their state by index, QMF bands start at zero
      lowidx+qmf  low hybrid bands by index, QMF bands by QMF band
      lowfreq     low hybrid bands from the old band of nearest frequency, QMF bands zero
    and 'duck': 'index' (parameter-band state kept by index) or 'reset'."""

    def switch(self, cfg):
        pol = self.policy['dec']
        if pol not in ('lowidx', 'lowidx+qmf', 'lowfreq') or self.dec is None:
            return super().switch(cfg)
        new = C.StreamDecorrelator(cfg)
        old = self.dec
        olow = [j for j, b in enumerate(old.bands) if b[3] == 'low']
        nlow = [j for j, b in enumerate(new.bands) if b[3] == 'low']
        pairs = []
        if pol == 'lowfreq':
            for jn in nlow:
                f = new.bands[jn][2]
                pairs.append((min(olow, key=lambda j: abs(old.bands[j][2] - f)), jn))
        else:
            pairs += list(zip(olow, nlow))
        if pol == 'lowidx+qmf':
            okey = {k: j for j, (k, b, f, kd) in enumerate(old.bands) if kd == 'qmf'}
            pairs += [(okey[k], j) for j, (k, b, f, kd) in enumerate(new.bands) if kd == 'qmf' and k in okey]
        for jo, jn in pairs:
            new.hin[:, jn] = old.hin[:, jo]
            for m in range(3):
                new.sx[m][:, jn] = old.sx[m][:, jo]
                new.sy[m][:, jn] = old.sy[m][:, jo]
        if self.policy.get('duck', 'index') == 'index':
            n = min(old.npar, new.npar)
            new.pk[:n], new.e1[:n], new.e2[:n] = old.pk[:n], old.e1[:n], old.e2[:n]
        self.dec = new
        self.cfg = cfg

    def run(self, X, frames):
        """As compose.Composer.run; with policy 'corner': 'stay', a frame whose layout
        differs from the last frame run without a registered change runs in the layout of
        the last frame run, its matrices mapped onto it (20 -> 34: MAP20_34 rows, a pair
        as the entry mean; 34 -> 20: the CONV_34_TO_20 means)."""
        n = len(X)
        L = np.array(X, complex)
        R = np.array(X, complex)
        self.outs = []
        self.trace = {}
        for f, fr in enumerate(frames):
            if 32 * f >= n:
                break
            o = self.tl.frame(fr)
            self.outs.append(o)
            if not o.processed:
                continue
            H, cfg = o.H, o.cfg
            if self.cfg is not None and cfg != self.cfg and not o.changed and self.policy.get('corner') == 'stay':
                H, cfg = remap_rows(H, cfg, self.cfg), self.cfg
            elif cfg != self.cfg or o.changed:
                self.switch(cfg)
            s0 = 32 * f
            win = np.zeros((44, 64), complex)
            win[:6] = self.hist
            seg = X[s0:s0 + 38]
            win[6:6 + len(seg)] = seg
            Hall = C.hybrid_window(win, cfg)[6:38]
            self.hist = X[s0 + 26:s0 + 32].copy()
            D = self.dec.process(Hall)
            self.trace[f] = (Hall, D, self.dec.par.copy(), cfg)
            par = self.dec.par
            h = H[:, par, :]
            Lb = h[:, :, 0] * Hall + h[:, :, 1] * D
            Rb = h[:, :, 2] * Hall + h[:, :, 3] * D
            m = min(32, n - s0)
            L[s0:s0 + m] = C.DM.to_qmf(Lb, cfg)[:m]
            R[s0:s0 + m] = C.DM.to_qmf(Rb, cfg)[:m]
        return L, R


class CarryComposer(Composer):
    """Composer whose decorrelator is a FormDecorrelator (policy 'form'). A registered
    20 <-> 34 change restarts it from zero (t26). A frame that runs in another layout than
    the last frame run without a registered change (a change registered while the PS was
    off) carries the state over by `carry`:
      'reset'  everything from zero (compose's rule)
      'index'  every per-band array (input history, all-pass state) by band index in the
               decorrelator's band order, `duck` 'index' the ducker's arrays by parameter
               band index
      a callable (old, new) -> [(old index, new index)] for the per-band arrays."""

    def switch(self, cfg, registered=True):
        pol = self.policy
        new = FormDecorrelator(cfg, pol.get('form', 'df1'))
        old = self.dec
        carry = pol.get('carry', 'reset')
        if old is not None and not registered:
            import copy
            self.old_dec = copy.deepcopy(old)
        if old is not None and not registered and carry != 'reset':
            if carry == 'index':
                pairs = [(j, j) for j in range(min(len(old.bands), len(new.bands)))]
            else:
                pairs = carry(old, new)
            for jo, jn in pairs:
                new.hin[:, jn] = old.hin[:, jo]
                for m in range(3):
                    new.sx[m][:, jn] = old.sx[m][:, jo]
                    new.sy[m][:, jn] = old.sy[m][:, jo]
                    new.w[m][:, jn] = old.w[m][:, jo]
            if pol.get('duck', 'reset') == 'index':
                n = min(old.npar, new.npar)
                new.pk[:n], new.e1[:n], new.e2[:n] = old.pk[:n], old.e1[:n], old.e2[:n]
            if 'duckzero' in pol:
                # parameter bands a-b of the ducker from zero
                a, b = (int(v) for v in pol['duckzero'].split('-'))
                new.pk[a:b + 1] = new.e1[a:b + 1] = new.e2[a:b + 1] = 0
            for part in pol.get('zero', '').split('+'):
                # parts of the carried state set to zero: 'ap' the all-pass sections,
                # 'hin' the input history, 'lowap' the sections of the low hybrid bands
                low = np.array([b[3] == 'low' for b in new.bands])
                for m in range(3):
                    if part == 'ap':
                        new.sx[m][:], new.sy[m][:], new.w[m][:] = 0, 0, 0
                    if part == 'lowap':
                        new.sx[m][:, low], new.sy[m][:, low], new.w[m][:, low] = 0, 0, 0
                if part == 'hin':
                    new.hin[:] = 0
                if part == 'lowhin':
                    new.hin[:, low] = 0
                if part == 'low':
                    new.hin[:, low] = 0
                    for m in range(3):
                        new.sx[m][:, low], new.sy[m][:, low], new.w[m][:, low] = 0, 0, 0
                if part == 'oldplain':
                    # sections of bands whose source band did not run the all-pass
                    src = dict((jn, jo) for jo, jn in pairs)
                    plain = np.array([src.get(j) is None or old.kind[src[j]] != 0 for j in range(len(new.bands))])
                    for m in range(3):
                        new.sx[m][:, plain], new.sy[m][:, plain], new.w[m][:, plain] = 0, 0, 0
        self.dec = new
        self.cfg = cfg

    def run(self, X, frames):
        n = len(X)
        L = np.array(X, complex)
        R = np.array(X, complex)
        self.outs = []
        self.trace = {}
        for f, fr in enumerate(frames):
            if 32 * f >= n:
                break
            o = self.tl.frame(fr)
            self.outs.append(o)
            if not o.processed:
                continue
            H, cfg = o.H, o.cfg
            if o.changed or self.dec is None:
                self.switch(cfg)
            elif cfg != self.cfg:
                self.corner = getattr(self, 'corner', []) + [f]
                self.switch(cfg, registered=False)
            s0 = 32 * f
            win = np.zeros((44, 64), complex)
            win[:6] = self.hist
            hp = self.policy.get('hist', 'keep')
            if hp != 'keep' and f in getattr(self, 'corner', []):
                # the 6 slots the hybrid analysis reads before a corner frame
                win[:6] = 0 if hp == 'zero' else X[s0 - 6:s0]
            seg = X[s0:s0 + 38]
            win[6:6 + len(seg)] = seg
            Hall = C.hybrid_window(win, cfg)[6:38]
            self.hist = X[s0 + 26:s0 + 32].copy()
            D = self.dec.process(Hall)
            self.trace[f] = (Hall, D, self.dec.par.copy(), cfg)
            par = self.dec.par
            h = H[:, par, :]
            Lb = h[:, :, 0] * Hall + h[:, :, 1] * D
            Rb = h[:, :, 2] * Hall + h[:, :, 3] * D
            m = min(32, n - s0)
            L[s0:s0 + m] = C.DM.to_qmf(Lb, cfg)[:m]
            R[s0:s0 + m] = C.DM.to_qmf(Rb, cfg)[:m]
        return L, R


# The decorrelator's per-band arrays as the reference keeps them (o12): one slot per band
# in a fixed order per layout, 71 (20 bands) or 91 (34 bands). The 34-band order is
# decmodel's (QMF 0 sub-bands m = 0..11, QMF 1 m = 0..7, QMF 2..4 m = 0..3, then QMF 5..63);
# the 20-band order lists QMF 0's bands by frequency: sub-band 6 (-0.375), 7 (-0.125),
# 0 (+0.125), 1 (+0.375), 2+5 (0.625), 3+4 (0.875); then QMF 1 (1, 2), (0, 3); QMF 2
# (0, 3), (1, 2); QMF 3..63. REF_SLOT[cfg][j] is the slot of decmodel band j.
REF_SLOT = {34: list(range(91)), 20: [2, 1, 3, 0, 4, 5] + list(range(6, 71))}
NSLOT = 91


class PersistComposer(Composer):
    """The composed model with the decorrelator's arrays kept per slot (REF_SLOT) over the
    whole stream: a 14-slot input history per slot, the lattice sections' state per slot
    and the ducker's three values per parameter band (34). A frame that runs reads the
    slots of its layout and writes back what it updated: the input history of every band,
    the section state of the all-pass bands only (policy 'secwrite': 'ap'; 'all' writes
    every band's), the ducker's values of its parameter bands. A registered 20 <-> 34
    change (t26) sets every array to zero first (policy 'regzero': 'all'; 'layout' only
    the new layout's slots)."""

    def __init__(self, policy=None, conv=None):
        super().__init__(policy, conv)
        if self.policy.get('form', 'lat') == 'df1':
            raise ValueError('the slots keep one state array per section: not the df1 form')
        self.p_hin = np.zeros((C.DM.LONG, NSLOT), complex)
        self.p_w = [np.zeros((d, NSLOT), complex) for d in C.DM.DELAYS]
        self.p_duck = np.zeros((3, 34))

    def zero(self, cfg=None):
        sl = slice(None) if cfg is None else REF_SLOT[cfg]
        self.p_hin[:, sl] = 0
        for w in self.p_w:
            w[:, sl] = 0
        if cfg is None:
            self.p_duck[:] = 0
        else:
            self.p_duck[:, :20 if cfg == 20 else 34] = 0

    def run(self, X, frames):
        n = len(X)
        L = np.array(X, complex)
        R = np.array(X, complex)
        self.outs = []
        self.trace = {}
        self.corner = []
        form = self.policy.get('form', 'lat')
        last = None
        for f, fr in enumerate(frames):
            if 32 * f >= n:
                break
            o = self.tl.frame(fr)
            self.outs.append(o)
            if not o.processed:
                continue
            H, cfg = o.H, o.cfg
            if o.changed:
                self.zero(None if self.policy.get('regzero', 'all') == 'all' else cfg)
            elif last is not None and cfg != last:
                self.corner.append(f)
            last = cfg
            dec = FormDecorrelator(cfg, form)
            sl = REF_SLOT[cfg]
            dec.hin[:] = self.p_hin[:, sl]
            for m in range(3):
                dec.w[m][:] = self.p_w[m][:, sl]
            dec.pk[:], dec.e1[:], dec.e2[:] = self.p_duck[:, :dec.npar]
            s0 = 32 * f
            win = np.zeros((44, 64), complex)
            win[:6] = self.hist
            seg = X[s0:s0 + 38]
            win[6:6 + len(seg)] = seg
            Hall = C.hybrid_window(win, cfg)[6:38]
            self.hist = X[s0 + 26:s0 + 32].copy()
            D = dec.process(Hall)
            self.p_hin[:, sl] = dec.hin
            ap = dec.kind == 0 if self.policy.get('secwrite', 'ap') == 'ap' else np.ones(len(sl), bool)
            for m in range(3):
                cols = np.array(sl)[ap]
                self.p_w[m][:, cols] = dec.w[m][:, ap]
            self.p_duck[:, :dec.npar] = dec.pk, dec.e1, dec.e2
            if self.policy.get('stale') == 'zero':
                # variant: what the frame's layout does not use is cleared
                used = np.zeros(NSLOT, bool)
                used[np.array(sl)[ap]] = True
                for m in range(3):
                    self.p_w[m][:, ~used] = 0
                rest = np.ones(NSLOT, bool)
                rest[sl] = False
                self.p_hin[:, rest] = 0
                self.p_duck[:, dec.npar:] = 0
            self.trace[f] = (Hall, D, dec.par.copy(), cfg)
            par = dec.par
            h = H[:, par, :]
            Lb = h[:, :, 0] * Hall + h[:, :, 1] * D
            Rb = h[:, :, 2] * Hall + h[:, :, 3] * D
            m = min(32, n - s0)
            L[s0:s0 + m] = C.DM.to_qmf(Lb, cfg)[:m]
            R[s0:s0 + m] = C.DM.to_qmf(Rb, cfg)[:m]
        return L, R


def remap_rows(H, cfg, to):
    """Per-slot matrices H (32, cfg, 4) as rows of the other layout."""
    if cfg == to:
        return H
    if to == 34:
        rows = [(m, m) if isinstance(m, int) else m for m in C.HM.MAP20_34]
        return np.stack([(H[:, a] + H[:, b]) / 2 for a, b in rows], 1)
    return np.stack([H[:, list(C.TM.CONV_34_TO_20[b])].mean(1) for b in range(20)], 1)
