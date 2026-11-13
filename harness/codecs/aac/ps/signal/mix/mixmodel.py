"""Mixing of the reference's Parametric Stereo decoder, as recovered by the `mix` probes
(see harness/codecs/aac/ps/README.md for the probe behind each rule and value).

Output channels of one hybrid band: L = h11 M + h12 D, R = h21 M + h22 D, with M the hybrid
band of the PS input and D its decorrelated signal.

- `matrix_real(iid, icc, fine, proc)`: the real 2x2 matrix per parameter band for IID / ICC
  indices; proc 'A' for ICC modes 0..2, 'B' for ICC modes 3..5.
- `PhaseSmoother`: the IPD/OPD smoothing state (one per configured parameter band).
- `matrix(...)`: the complex matrix of one envelope (real matrix times the row phases).
- `Interp`: the per-slot matrix of every hybrid band across envelopes (linear ramps of the
  real and imaginary parts, with the IPD on/off and image-band rules).
- `Mixer`: a whole-stream model on top of the hyb recovery's hybrid filterbank (verification).
"""
import numpy as np

SQ2 = np.sqrt(2.0)
IID_COARSE = [0, 2, 4, 7, 10, 14, 18, 25]                                         # dB
IID_FINE = [0, 2, 4, 6, 8, 10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50]          # dB
# rho = cos(2 alpha) per ICC index (m04: fitted to 3e-8 from 120 + 248 matrices each).
ICC_RHO = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0]
RHO_MIN_B = 0.05          # second procedure: rho is raised to at least 0.05 (m04)
IPD_STEP = np.pi / 4      # IPD / OPD index step (m06, m07: exact)
SMOOTH_W = (4.0, 2.0, 1.0)  # weights of the current and two previous envelopes (m07)
NIPD = {10: 5, 20: 11, 34: 17}   # IPD/OPD bands by the IID band count
# Hybrid bands whose interpolation starts from the conjugate of the previous matrix
# (m20, m26): (configuration, QMF band, sub-band of the Q-band split).
IMAGE_SUBBANDS = {20: {(0, 6), (0, 7)}, 34: {(0, 9), (0, 10), (0, 11), (1, 0), (1, 1)}}


def iid_db(idx, fine):
    g = IID_FINE if fine else IID_COARSE
    idx = np.asarray(idx)
    return np.sign(idx) * np.asarray(g)[np.abs(idx)]


def iid_gains(idx, fine):
    """(cl, cr) = (sqrt2 c, sqrt2) / sqrt(1 + c^2), c = 10^(IID/20) (Purnhagen eq. 5)."""
    c = 10.0 ** (iid_db(idx, fine) / 20.0)
    return SQ2 * c / np.sqrt(1 + c * c), SQ2 / np.sqrt(1 + c * c)


def matrix_real(iid, icc, fine=False, proc='A'):
    """(4, nb) array h11, h12, h21, h22 for per-band IID and ICC indices.

    A (ICC modes 0..2): Purnhagen 2004 eq. 5 with eq. 8 for beta:
        alpha = arccos(rho) / 2, beta = alpha (cr - cl) / sqrt2,
        H = [[cl cos(beta + alpha), cl sin(beta + alpha)], [cr cos(beta - alpha), cr sin(beta - alpha)]].
    B (ICC modes 3..5): Breebaart 2005 eq. 24-29 without phases, rho >= 0.05:
        a = atan(2 c rho / (c^2 - 1)) / 2 mod pi/2 (pi/4 when c = 1),
        mu^2 = 1 + (4 rho^2 - 4) / (c + 1/c)^2 rounded to float32, mu = float32(sqrt(mu^2)),
        g = atan(sqrt((1 - mu) / (1 + mu))),
        H = sqrt2 [[cos a cos g, -sin a sin g], [sin a cos g, cos a sin g]].
    """
    iid = np.asarray(iid, int)
    icc = np.asarray(icc, int)
    rho = np.asarray(ICC_RHO)[icc]
    if proc == 'A':
        cl, cr = iid_gains(iid, fine)
        al = np.arccos(rho) / 2
        be = al * (cr - cl) / SQ2
        return np.array([cl * np.cos(be + al), cl * np.sin(be + al), cr * np.cos(be - al), cr * np.sin(be - al)])
    rho = np.maximum(rho, RHO_MIN_B)
    c = 10.0 ** (iid_db(iid, fine) / 20.0)
    with np.errstate(divide='ignore', invalid='ignore'):
        a = np.where(iid == 0, np.pi / 4, np.mod(0.5 * np.arctan(2 * c * rho / (c * c - 1)), np.pi / 2))
    q = c + 1 / c
    mu2 = (1 + (4 * rho * rho - 4) / (q * q)).astype(np.float32).astype(np.float64)
    mu = np.sqrt(mu2).astype(np.float32).astype(np.float64)
    g = np.arctan(np.sqrt((1 - mu) / (1 + mu)))
    return SQ2 * np.array([np.cos(a) * np.cos(g), -np.sin(a) * np.sin(g), np.sin(a) * np.cos(g), np.cos(a) * np.sin(g)])


class PhaseSmoother:
    """IPD/OPD smoothing per configured parameter band (m07, m27): the phase applied is
    arg(4 u_0 + 2 u_1 + u_2) for the unit phasors u of the indices of the current and the two
    previous envelopes, separately for IPD and OPD. The history starts at index 0 and is
    not advanced by envelopes without IPD/OPD (m28)."""

    def __init__(self, nb):
        self.ipd = np.zeros((2, nb))   # previous two envelopes, newest first (radians)
        self.opd = np.zeros((2, nb))

    def step(self, ipd_idx, opd_idx):
        """Advance by one envelope; returns (phase_L, phase_R) in radians per band."""
        out = []
        for hist, idx in ((self.ipd, ipd_idx), (self.opd, opd_idx)):
            cur = np.asarray(idx, float) * IPD_STEP
            z = SMOOTH_W[0] * np.exp(1j * cur) + SMOOTH_W[1] * np.exp(1j * hist[0]) + SMOOTH_W[2] * np.exp(1j * hist[1])
            hist[1] = hist[0]
            hist[0] = cur
            out.append(np.angle(z))
        ipd_s, opd_s = out
        return opd_s, opd_s - ipd_s


def matrix(iid, icc, fine=False, proc='A', phases=None):
    """(4, nb) complex matrix of one envelope: the real matrix with row 1 times exp(j phase_L)
    and row 2 times exp(j phase_R) (m08: the phase multiplies both columns of its row).
    phases: (phase_L, phase_R) from PhaseSmoother.step, or None."""
    h = matrix_real(iid, icc, fine, proc).astype(complex)
    if phases is not None:
        pl, pr = (np.exp(1j * p) for p in phases)
        h[0] *= pl
        h[1] *= pl
        h[2] *= pr
        h[3] *= pr
    return h


class Interp:
    """Per-slot matrices of every hybrid band (m07, m19, m20, m28, m30).

    In each envelope (slots s0 < t <= s1, s1 the envelope's last slot) every matrix entry is a
    straight line from a start value to the envelope's own value, reached at s1:
    h(t) = h0 + (h1 - h0) (t - s0) / (s1 - s0). The matrices are stored per envelope index
    (0..3), real and imaginary parts apart:
    - the start of envelope e > 0 is the stored matrix of envelope e - 1 (of this frame); the
      start of envelope 0 is the stored matrix of the previous frame's last envelope index;
    - envelopes without IPD/OPD use the real part only and store only the real part, so a
      later envelope with IPD/OPD can start from an older imaginary part (m28, m30);
    - image hybrid bands (IMAGE_SUBBANDS) start from the negated stored imaginary part, in
      all four entries (m33: the D column too).
    The store starts at the zero matrix, so the first PS frame ramps from zero (the time recovery).
    """

    NE = 4

    def __init__(self, par, image, stale_im=True, image_cols=(0, 1, 2, 3)):
        self.par = np.asarray(par)           # parameter band of each hybrid band
        self.image = np.asarray(image, bool)
        self.stale_im = stale_im             # False: envelopes without IPD/OPD store Im = 0 (ablation)
        self.image_cols = np.isin(np.arange(4), image_cols)[:, None]  # entries the image rule negates
        self.re = None                       # (NE, 4, nh)
        self.im = None
        self.prev_last = 0

    def envelope(self, h, n, has_phase, e, last=False):
        """h: (4, nb) complex matrix of envelope e of the frame, n: its length in slots.
        Returns (n, 4, nh) complex per-slot matrices. last: e is the frame's last envelope."""
        hb = h[:, self.par]
        re1, im1 = hb.real.copy(), hb.imag.copy()
        if self.re is None:                       # first PS frame: the zero matrix
            self.re = np.zeros((self.NE,) + re1.shape)
            self.im = np.zeros((self.NE,) + re1.shape)
        s = e - 1 if e > 0 else self.prev_last
        re0, im0 = self.re[s], np.where(self.image[None, :] & self.image_cols, -self.im[s], self.im[s])
        w = (np.arange(1, n + 1) / n)[:, None, None]
        re = re0[None] + (re1 - re0)[None] * w
        self.re[e] = re1
        if has_phase:
            im = im0[None] + (im1 - im0)[None] * w
            self.im[e] = im1
        else:
            im = 0.0
            if not self.stale_im:
                self.im[e] = 0.0
        if last:
            self.prev_last = e
        return re + 1j * im


def hybrid_layout(cfg, HM, image=True):
    """[(QMF band, taps or None, parameter band, image)] per hybrid band: the hyb recovery's bands, with
    the mirror pairs that hold an image sub-band split into their two sub-bands. image=False
    drops the image rule (ablation)."""
    out = []
    for k, Q, S, b in HM.LOW[cfg]:
        img = [m for m in S if (k, m) in IMAGE_SUBBANDS[cfg]]
        if img:
            for m in S:
                out.append((k, HM.filt(Q, (m,), cfg), b, image and (k, m) in IMAGE_SUBBANDS[cfg]))
        else:
            out.append((k, HM.filt(Q, S, cfg), b, False))
    out += [(k, None, b, False) for k, b in sorted(HM.qmf_par(cfg).items())]
    return out


def dec_layout(cfg, HM, DM, image=True):
    """the dec recovery's decorrelator bands (71 / 91: decmodel.bands order) as mixing bands, with
    the image flag per band; D from decmodel.Decorrelator lines up with it."""
    out = [(k, HM.filt(Q, S, cfg), b, image and len(S) == 1 and (k, S[0]) in IMAGE_SUBBANDS[cfg])
           for k, Q, S, b, f in DM.LOW[cfg]]
    out += [(k, None, b, False) for k, b in sorted(HM.qmf_par(cfg).items())]
    return out


def to_config_ipd(idx, nb_iid, cfg, HM):
    """IPD or OPD indices of the coded IID resolution (NIPD[nb_iid] bands, bands beyond are 0)
    onto the configuration: the same maps as IID (the hyb recovery; m21, m29: pair bands take the
    integer index mean truncated toward zero, not a circular mean)."""
    full = list(idx) + [0] * (nb_iid - len(idx))
    return np.asarray(HM.to_config(full, nb_iid, cfg))


class Mixer:
    """Whole-stream model for verification: per frame a dict with keys
    iid, icc (coded indices per envelope), iid_mode, icc_mode (None when disabled), nenv
    (1, 2 or 4, frame_class 0), ext ('ipd': extension with IPD/OPD, 'off': extension with the
    IPD/OPD enable bit 0, None: no extension) and ipd, opd (per envelope coded indices, for
    ext 'ipd'). D must be given per hybrid band.
    Only what the mix probes cover: frame_class 0, the configuration fixed over the stream."""

    def __init__(self, cfg, HM, off_clears=True, image=True, stale_im=True, per_env=True, DM=None,
                 image_cols=(0, 1, 2, 3), d_phase='row'):
        """The keyword flags switch single rules off for ablation checks: off_clears (an
        extension with the enable bit 0 clears the stored IPD/OPD values), image (image hybrid
        bands start from the conjugate), stale_im (envelopes without IPD/OPD keep the stored
        imaginary part), per_env (frames without the extension reuse the values stored for
        each envelope index; False: the last envelope's values for every envelope).
        DM: the dec recovery's decmodel; given, the bands are the decorrelator's (dec_layout), so that
        D = DM.Decorrelator(cfg).process(DM.analysis(X, cfg)) lines up. image_cols: the
        entries the image rule negates; d_phase 'row' (D column turned with its row) or
        'conj' (D column turned the opposite way) - the last two for checks only."""
        self.cfg, self.HM = cfg, HM
        self.off_clears, self.per_env, self.d_phase = off_clears, per_env, d_phase
        lay = dec_layout(cfg, HM, DM, image) if DM is not None else hybrid_layout(cfg, HM, image)
        self.lay = lay
        self.interp = Interp([b for _, _, b, _ in lay], [im for _, _, _, im in lay], stale_im, image_cols)
        self.smoother = PhaseSmoother(cfg)
        self.iid_mode = None
        self.icc_mode = None
        self.seen_iid = False
        self.iid_nb = 10
        self.ipd_on = False
        self.last_e = 0
        self.ipd_cfg = np.zeros((Interp.NE, cfg), int)   # indices on the configuration, per
        self.opd_cfg = np.zeros((Interp.NE, cfg), int)   # envelope index

    def analysis(self, X):
        return np.stack([X[:, k] if f is None else self.HM._fir(X[:, k], f) for k, f, _, _ in self.lay], 1)

    def synthesis(self, H):
        out = np.zeros((len(H), 64), complex)
        for j, (k, _, _, _) in enumerate(self.lay):
            out[:, k] += H[:, j]
        return out

    def frame_matrices(self, fr):
        """(32, 4, nh) per-slot matrices of one frame."""
        HM, cfg = self.HM, self.cfg
        if fr.get('iid_mode', 'keep') != 'keep':
            self.iid_mode = fr['iid_mode']
        if fr.get('icc_mode', 'keep') != 'keep':
            self.icc_mode = fr['icc_mode']
        if self.iid_mode is not None:
            self.seen_iid = True
            self.iid_nb = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}[self.iid_mode]
        nenv = fr['nenv']
        fine = self.iid_mode is not None and self.iid_mode >= 3
        proc = 'B' if (self.icc_mode is not None and self.icc_mode >= 3) else 'A'
        mats = []
        for e in range(nenv):
            if self.iid_mode is None:
                iid = np.zeros(cfg, int)
            else:
                iid = HM.to_config(fr['iid'][e], self.iid_nb, cfg)
            if self.icc_mode is None:
                icc = np.zeros(cfg, int)
            else:
                icc = HM.to_config(fr['icc'][e], {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}[self.icc_mode], cfg)
            ph = None
            if fr.get('ext') == 'ipd':                 # extension with IPD/OPD: store per envelope
                self.ipd_on = True
                self.ipd_cfg[e] = to_config_ipd(fr['ipd'][e], self.iid_nb, cfg, HM)
                self.opd_cfg[e] = to_config_ipd(fr['opd'][e], self.iid_nb, cfg, HM)
                self.last_e = e
            elif fr.get('ext') == 'off' and e == 0:    # extension with the enable bit 0
                self.ipd_on = False
                if self.off_clears:
                    self.ipd_cfg[:] = 0
                    self.opd_cfg[:] = 0
            elif fr.get('ext') is None and not self.per_env:
                self.ipd_cfg[e], self.opd_cfg[e] = self.ipd_cfg[self.last_e], self.opd_cfg[self.last_e]
            # without the extension: the last extension's enable bit, and the values stored for
            # this envelope index (m30, v_mix)
            has = self.ipd_on and self.seen_iid
            if has:
                ph = self.smoother.step(self.ipd_cfg[e], self.opd_cfg[e])
            h = matrix(iid, icc, fine, proc, ph)
            if ph is not None and self.d_phase == 'conj':
                h[1] *= np.exp(-2j * ph[0])
                h[3] *= np.exp(-2j * ph[1])
            mats.append(self.interp.envelope(h, 32 // nenv, has, e,
                                             last=e == nenv - 1))
        return np.concatenate(mats, 0)

    def run(self, X, D, frames):
        """X: (n, 64) PS input slots, D: (n, nh) decorrelated hybrid slots. Returns QMF slots
        (L, R), each (n, 64)."""
        M = self.analysis(X)
        n = min(len(M), 32 * len(frames))
        L = np.zeros((n, M.shape[1]), complex)
        R = np.zeros_like(L)
        for f, fr in enumerate(frames):
            s = slice(32 * f, 32 * f + 32)
            if 32 * f >= n:
                break
            h = self.frame_matrices(fr)[: n - 32 * f]
            L[s] = h[:, 0] * M[s] + h[:, 1] * D[s]
            R[s] = h[:, 2] * M[s] + h[:, 3] * D[s]
        return self.synthesis(L), self.synthesis(R)
