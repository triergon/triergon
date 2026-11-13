"""Whole-stream PS model composed from the models of the other parts: the hyb recovery's hybrid filters
(hybmodel), the dec recovery's decorrelator and ducking (decmodel; run here as a streaming filter
with explicit state so that frames the PS does not run can be skipped and the state can be
carried over a 20 <-> 34 change), the mix recovery's real matrices (mixmodel.matrix_real; no
IPD/OPD here), and this part's timeline (timemodel).

Per frame the PS either runs (hybrid analysis, decorrelator, per-slot matrices per
parameter band, synthesis back into QMF bands) or outputs the mono input on both channels
without touching any state. Mixing is done in the decorrelator's band layout (71 / 91
bands, each with its parameter band); with real matrices this equals mixing in the hyb recovery's
layout.

`Composer(policy).run(X, frames)` -> (L, R) QMF slots. The default policy is the
reference's (t26, t27, verify_compose): at a frame the timeline marks as changed (a 20 <-> 34
change, or a frame without PS repeating one) the decorrelator and ducking start from zero;
the hybrid history (the 6 QMF slots before the frame, from the last frame the PS ran) is
kept. The other policies are the alternatives the probes rule out (t26)."""
import os
import sys

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
for part in ('hyb', 'dec', 'mix'):
    p = os.path.join(HERE, "..", part)
    if p not in sys.path:
        sys.path.insert(0, p)
import hybmodel as HM  # noqa: E402
import decmodel as DM  # noqa: E402
import mixmodel as MM  # noqa: E402
import timemodel as TM  # noqa: E402

NB = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}


def matrices(iid, icc, iid_mode, icc_mode, cfg):
    """timemodel callback: (nb_cfg, 4) real matrices from coded indices (the hyb recovery's maps,
    the mix recovery's matrices)."""
    ni = cfg if iid is None else len(iid)
    nc = cfg if icc is None else len(icc)
    a = np.zeros(cfg, int) if iid is None else np.asarray(HM.to_config(iid, ni, cfg))
    c = np.zeros(cfg, int) if icc is None else np.asarray(HM.to_config(icc, nc, cfg))
    fine = iid_mode is not None and iid_mode >= 3
    proc = 'B' if (icc_mode is not None and icc_mode >= 3) else 'A'
    return MM.matrix_real(a, c, fine, proc).T


class StreamDecorrelator:
    """decmodel's decorrelator and ducker as a slot-by-slot filter with explicit state
    (direct form: each all-pass section keeps its last d inputs and outputs)."""

    def __init__(self, cfg):
        self.cfg = cfg
        self.bands = DM.bands(cfg)
        nb = len(self.bands)
        self.kind = np.zeros(nb, int)           # 0 all-pass, 1 14-slot delay, 2 1-slot delay
        f = np.zeros(nb)
        s = np.ones(nb)
        for j, (k, b, ff, kd) in enumerate(self.bands):
            f[j] = ff
            if kd == 'qmf' and k >= DM.SHORT_START:
                self.kind[j] = 2
            elif kd == 'qmf' and k >= DM.AP_END:
                self.kind[j] = 1
            elif kd == 'qmf':
                s[j] = 1 - DM.SLOPE * (k - DM.NLOW[cfg])
        self.phi = np.exp(-1j * np.pi * DM.F0 * f)
        self.a = [DM.A[m] * s for m in range(3)]
        self.q = [np.exp(-1j * np.pi * DM.QF[m] * f) for m in range(3)]
        self.par = np.array([b for _, b, _, _ in self.bands])
        self.npar = 1 + self.par.max()
        self.reset()

    def reset(self):
        nb = len(self.bands)
        self.hin = np.zeros((DM.LONG, nb), complex)          # last 14 inputs, newest first
        self.sx = [np.zeros((d, nb), complex) for d in DM.DELAYS]   # section inputs, newest first
        self.sy = [np.zeros((d, nb), complex) for d in DM.DELAYS]
        self.pk = np.zeros(self.npar)
        self.e1 = np.zeros(self.npar)
        self.e2 = np.zeros(self.npar)

    def process(self, H):
        out = np.zeros_like(H)
        ap = self.kind == 0
        for t in range(len(H)):
            x = H[t]
            u = self.phi * self.hin[DM.PRE - 1]
            for m, d in enumerate(DM.DELAYS):
                y = -self.a[m] * u + self.q[m] * self.sx[m][d - 1] + self.a[m] * self.q[m] * self.sy[m][d - 1]
                self.sx[m] = np.roll(self.sx[m], 1, 0)
                self.sx[m][0] = u
                self.sy[m] = np.roll(self.sy[m], 1, 0)
                self.sy[m][0] = y
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
            out[t] = D * g[self.par]
        return out


def hybrid_window(Xwin, cfg):
    """Decorrelator-layout hybrid bands of a window of QMF slots."""
    return DM.analysis(Xwin, cfg)


class Composer:
    """policy: dict of rules at a 20 <-> 34 change (keys 'dec': 'reset' | 'qmf' | 'index',
    'hist': 'keep' | 'zero')."""

    def __init__(self, policy=None, conv=None):
        self.policy = dict({'dec': 'reset', 'duck': 'reset', 'hist': 'keep', 'trigger': 'conv'}, **(policy or {}))
        self.tl = TM.Timeline(matrices, conv=conv)
        if self.policy.get('lencfg') == '0':
            self.tl.length_sets_cfg = False
        if self.policy.get('init20') == '1':
            self.tl.hdr_cfg = 20
        if 'bandmap' in self.policy:
            self.tl.band_change = self.policy['bandmap']
        if self.policy.get('rocfg') == '0':
            self.tl.ro_sets_cfg = False
        self.dec = None
        self.cfg = None
        self.hist = np.zeros((6, 64), complex)      # X slots before the next frame the PS runs

    def switch(self, cfg):
        """New decorrelator for cfg, with the state carried by the policy."""
        pol = self.policy['dec']
        if pol == 'separate':      # one decorrelator per configuration, kept while unused
            self.decs = getattr(self, 'decs', {})
            if self.dec is not None:
                self.decs[self.cfg] = self.dec
            self.dec = self.decs.get(cfg) or StreamDecorrelator(cfg)
            self.cfg = cfg
            return
        new = StreamDecorrelator(cfg)
        old = self.dec
        if old is not None and pol != 'reset':
            if pol == 'index':
                n = min(len(old.bands), len(new.bands))
                pairs = [(j, j) for j in range(n)]
            elif pol == 'qmfall':   # every band by QMF band: hybrid bands of a QMF band too
                pairs = []
                for jn, (k, b, f, kd) in enumerate(new.bands):
                    js = [jo for jo, (ko, _, _, _) in enumerate(old.bands) if ko == k]
                    if js:
                        pairs.append((js[0], jn))
            else:   # by QMF band: plain QMF bands carry over, low hybrid bands start at zero
                okey = {(k, kd): j for j, (k, b, f, kd) in enumerate(old.bands) if kd == 'qmf'}
                pairs = [(okey[(k, kd)], j) for j, (k, b, f, kd) in enumerate(new.bands)
                         if kd == 'qmf' and (k, kd) in okey]
            for jo, jn in pairs:
                new.hin[:, jn] = old.hin[:, jo]
                for m in range(3):
                    new.sx[m][:, jn] = old.sx[m][:, jo]
                    new.sy[m][:, jn] = old.sy[m][:, jo]
        if old is not None and self.policy.get('duck', 'index') == 'index':
            n = min(old.npar, new.npar)
            new.pk[:n], new.e1[:n], new.e2[:n] = old.pk[:n], old.e1[:n], old.e2[:n]
        self.dec = new
        self.cfg = cfg
        if self.policy['hist'] == 'zero':
            self.hist[:] = 0

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
            trig = self.policy.get('trigger', 'cfg')
            if o.cfg != self.cfg or (trig == 'conv' and o.changed) or \
                    (trig == 'conv_ps' and o.changed and o.reason == 'ps'):
                self.switch(o.cfg)
            s0 = 32 * f
            win = np.zeros((44, 64), complex)
            win[:6] = self.hist
            if self.policy['hist'] == 'cont' and s0 >= 6:     # variant: true preceding slots
                win[:6] = X[s0 - 6:s0]
            seg = X[s0:s0 + 38]
            win[6:6 + len(seg)] = seg
            Hall = hybrid_window(win, o.cfg)[6:38]
            self.hist = X[s0 + 26:s0 + 32].copy()
            D = self.dec.process(Hall)
            self.trace[f] = (Hall, D, self.dec.par.copy())
            par = self.dec.par
            h = o.H[:, par, :]                       # (32, nb, 4)
            Lb = h[:, :, 0] * Hall + h[:, :, 1] * D
            Rb = h[:, :, 2] * Hall + h[:, :, 3] * D
            m = min(32, n - s0)
            L[s0:s0 + m] = DM.to_qmf(Lb, o.cfg)[:m]
            R[s0:s0 + m] = DM.to_qmf(Rb, o.cfg)[:m]
        return L, R
