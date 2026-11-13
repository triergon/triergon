"""Behaviour of the reference's Parametric Stereo over time, as recovered by the probes in
this directory (t01..t29, verify_time.py, verify_compose.py; notes in
harness/codecs/aac/ps/README.md). compose.py composes it with the hyb, dec and mix recoveries
into a whole-stream model.

The model turns the parsed PS data of each AAC frame into the per-slot mixing matrices
(h11, h12, h21, h22 per parameter band, L = h11 M + h12 D, R = h21 M + h22 D) and says,
per frame, whether the PS runs at all. When it does not run, the output is the mono QMF
input on both channels and no PS state (hybrid history, decorrelator, ducking) advances.
FrameOut.changed marks a frame that runs after a 20 <-> 34 change (or a frame without PS
repeating it): the decorrelator and ducking start from zero there (t26, verify_compose);
the 6 hybrid-history slots stay those of the last frame the PS ran.

Interface: `Timeline(matrices)`; `Timeline.frame(fr)` -> `FrameOut` per AAC frame, in
stream order. `matrices(iid, icc, iid_mode, icc_mode, cfg)` maps one envelope's absolute
indices to an array (nbands(cfg), 4) of matrix entries; `default_matrices` covers IID
alone and ICC alone (the mix recovery supplies the general one).

A frame `fr` is a dict:
  {'ps': False}                       no PS data in this frame (also a frame whose SBR
                                      element is missing or fails)
  {'ps': True, 'error': 'length', ...} the PS extension size does not match the data read
                                      (the caller's parser decides this); give the frame's
                                      other fields too: errors met while reading it come
                                      first
  {'ps': True, 'error': 'header'}     reserved iid/icc mode (nothing after the header read)
  {'ps': True, 'header': h or None, 'frame_class': 0 or 1, 'nenv': n, 'borders': [...],
   'iid': [(dt, values), ...] or None, 'icc': [...] or None, 'cfg': 20, 34 or None}
      h = {'iid_mode': m or None, 'icc_mode': m or None} (None: IID/ICC disabled);
      cfg: the hyb recovery's configuration of the frame's modes; None for a header with IID and
      ICC both disabled, which keeps the configuration (20 at the stream start) (t27);
      values: per band, the coded differences (dt False: along frequency from 0; dt True:
      against the previous envelope, or for envelope 0 against the last envelope of the
      previous PS frame); nenv 0 (frame_class 0) carries no values.
      Border order errors (a border below the previous one) and out-of-range results are
      detected here, as the reference does.
"""
import numpy as np

COARSE_DB = [-25, -18, -14, -10, -7, -4, -2, 0, 2, 4, 7, 10, 14, 18, 25]
FINE_DB = [-50, -45, -40, -35, -30, -25, -22, -19, -16, -13, -10, -8, -6, -4, -2, 0, 2, 4, 6,
           8, 10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50]
COS_ALPHA = [1.0, 0.9841240, 0.9594738, 0.8946843, 0.8269341, 0.7071068, 0.4533211, 0.0]
NBANDS = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}
SLOTS = 32

# Conversion of the start matrix read from the store when the configuration changes (t14,
# t25, recover_time.py). Key = band in the new configuration, value = the bands of the old
# configuration whose stored entries are averaged entry by entry (a band listed twice
# counts twice: b0 = (2 j0 + j1) / 3). 20 -> 34 is the hyb recovery's parameter map MAP20_34 with
# its two pairs as entry means; 34 -> 20 the means below.
CONV_34_TO_20 = {0: (0, 0, 1), 1: (1, 2, 2), 2: (3, 3, 4), 3: (4, 5, 5), 4: (6, 7), 5: (8, 9),
                 6: (10,), 7: (11,), 8: (12, 13), 9: (14, 15), 10: (16,),
                 11: (17,), 12: (18,), 13: (19,), 14: (20, 21), 15: (22, 23), 16: (24, 25),
                 17: (26, 27), 18: (28, 29, 30, 31), 19: (32, 33)}
CONV_20_TO_34 = {0: (0,), 1: (0, 1), 2: (1,), 3: (2,), 4: (2, 3), 5: (3,), 6: (4,), 7: (4,),
                 8: (5,), 9: (5,), 10: (6,), 11: (7,), 12: (8,), 13: (8,), 14: (9,), 15: (9,),
                 16: (10,),
                 17: (11,), 18: (12,), 19: (13,), 20: (14,), 21: (14,), 22: (15,), 23: (15,),
                 24: (16,), 25: (16,), 26: (17,), 27: (17,), 28: (18,), 29: (18,), 30: (18,),
                 31: (18,), 32: (19,), 33: (19,)}


def cl_cr(db):
    c = 10.0 ** (np.asarray(db, float) / 20)
    return np.sqrt(2) * c / np.sqrt(1 + c * c), np.sqrt(2) / np.sqrt(1 + c * c)


def default_matrices(iid, icc, iid_mode, icc_mode, cfg):
    """IID alone (ICC index 0) or ICC alone (IID index 0), with the coded band count equal
    to the configuration's. Entries [h11, h12, h21, h22] per band."""
    nb = cfg
    iid = np.zeros(nb, int) if iid is None else np.asarray(iid)
    icc = np.zeros(nb, int) if icc is None else np.asarray(icc)
    if len(iid) != nb or len(icc) != nb:
        raise NotImplementedError('band-count conversion belongs to the hybrid recovery')
    grid = FINE_DB if (iid_mode or 0) >= 3 else COARSE_DB
    off = 15 if (iid_mode or 0) >= 3 else 7
    H = np.zeros((nb, 4))
    for b in range(nb):
        if iid[b] != 0 and icc[b] != 0:
            raise NotImplementedError('IID with ICC: the mix recovery')
        l, r = cl_cr(grid[iid[b] + off])
        ca = COS_ALPHA[icc[b]]
        sa = np.sqrt(1 - ca * ca)
        if icc[b] == 0:
            H[b] = (l, 0.0, r, 0.0)
        else:
            H[b] = (ca, sa, ca, -sa)
    return H


def _keep(old, new):
    """Kept values (dt reference, num_env 0 target): one array of 34 per parameter; a frame
    writes its first n entries, the rest keep older values (verify_compose seed 8313, t29)."""
    if new is None:
        return old
    out = np.zeros(34, int) if old is None else np.array(old)
    out[:len(new)] = new
    return out


def borders(frame_class, nenv, coded):
    """Last slot of each envelope (t03): frame_class 0 splits the frame evenly, frame_class
    1 uses the coded 5-bit values as read; when its last border is below 31 the frame gets
    one more envelope, ending at slot 31 with the last envelope's values (t09, verify).
    nenv 0 is one envelope ending at slot 31."""
    if nenv == 0:
        return [SLOTS - 1]
    if frame_class == 0:
        return [SLOTS * (e + 1) // nenv - 1 for e in range(nenv)]
    b = list(coded)
    return b + [SLOTS - 1] if b[-1] < SLOTS - 1 else b


def interpolate(start, targets, bord):
    """Per-slot matrices (32, nb, 4): envelope e ramps linearly from the previous envelope's
    target (the start matrix for e = 0) to its own, reaching it at its last slot; after the
    last border the last target holds. A zero-length envelope only changes the value the
    next ramp starts from (t09)."""
    out = np.empty((SLOTS,) + start.shape)
    prev = start
    first = 0
    for T, b in zip(targets, bord):
        width = b - first + 1
        for t in range(first, b + 1):
            out[t] = prev + (T - prev) * ((t - first + 1) / width)
        prev = T
        first = max(first, b + 1)
    out[first:] = targets[-1]
    return out


class FrameOut:
    def __init__(self, processed, H=None, cfg=None, reason='', start=None, changed=False):
        self.processed = processed   # False: mono output, PS state not advanced
        self.H = H                   # (32, nbands, 4) when processed
        self.cfg = cfg
        self.reason = reason
        self.start = start           # the start matrix of the first envelope
        self.changed = changed       # the start was converted from the other layout


class Timeline:
    def __init__(self, matrices=default_matrices, conv=None):
        self.matrices = matrices
        self.conv = conv or {(34, 20): CONV_34_TO_20, (20, 34): CONV_20_TO_34}
        self.active = False          # a valid header has been read and no error since
        self.hdr_seen = False        # a PS header has been read at all
        self.length_sets_cfg = True  # a size-mismatch frame counts for the configuration
        self.ro_sets_cfg = True      # so does a header-less frame read while the PS is off
        self.band_change = 'index'   # kept values at another band count (t29)
        self.modes = {'iid_mode': None, 'icc_mode': None}
        # stored target matrices per envelope index, 34 rows each; a 20-band frame writes
        # and reads rows 0..19 (rows 20..33 keep what an earlier 34-band frame left)
        self.store = {}
        self.hdr_cfg = 20            # configuration of the last frame read to its end; 20 at the
        #                              start, so a first frame in 34 bands is a change (v_rust 9110)
        self.cfg_before = None       # ... of the one before it (t14, verify)
        self.n_last = 0              # envelope count of the last frame whose grid was read
        self.n_before = 0            # ... of the one before it
        self.last_iid = None         # last envelope's absolute indices (dt reference, nenv 0)
        self.last_icc = None
        self.last = None             # (borders, targets, cfg) of the last PS frame run

    # -- state helpers -----------------------------------------------------------------
    def _start(self, cfg, old=None):
        """Stored target at envelope index n_before - 1 (zero when nothing was stored
        there), read in the layout of cfg, or converted from the layout of `old` when the
        configuration changed with this frame. The store itself is not converted."""
        H = self.store.get(self.n_before - 1)
        if H is None:
            return np.zeros((cfg, 4))
        if old is None:
            return H[:cfg].copy()
        table = self.conv[(old, cfg)]
        M = np.zeros((cfg, 4))
        for b in range(cfg):
            if b not in table:
                raise NotImplementedError('conversion of band %d %d->%d: hybrid recovery map' % (b, old, cfg))
            M[b] = H[list(table[b])].mean(0)
        return M

    def _put(self, e, M, cfg):
        """Store M (cfg rows) at envelope index e; index -1 holds the start matrix of the
        last frame that ran, read when the count before is 0 (zero before any; t31)."""
        S = self.store.get(e, np.zeros((34, 4))).copy()
        S[:cfg] = M
        self.store[e] = S

    def _set_cfg(self, cfg):
        """A frame of configuration cfg was read to its end: a frame that runs, a frame
        refused for its size, or a header-less frame read while the PS is off (a frame
        refused while reading does not count). A frame that runs converts its start when
        its configuration differs from the one before; a frame without PS that repeats it
        converts again, from the store as that frame left it (verify seeds 1003, 3003,
        3008; verify_compose seeds 8002, 8112)."""
        self.cfg_before, self.hdr_cfg = self.hdr_cfg, cfg
        return self._changed()

    def _changed(self):
        prev = self.cfg_before
        return prev is not None and prev != self.hdr_cfg, prev

    def _error(self, fr=None):
        """An error frame (t08, t09, t11, t12, t17, t18, t20, t21, t23): mono output, no PS
        state advances; its header and grid (when read) still count."""
        if fr is not None and fr.get('header') is not None:
            self.hdr_seen = True
        if fr is not None and fr.get('cfg') is not None and fr.get('error') == 'length' and self.length_sets_cfg \
                and (fr.get('header') is not None or self.hdr_seen):
            # read to its end, with or without a header (v_rust TS 9202)
            self._set_cfg(fr['cfg'])
        if fr is not None and fr.get('nenv') is not None:
            if fr.get('error') == 'length':
                # the whole frame was read: its count includes the extra envelope (t21)
                n = len(borders(fr.get('frame_class', 0), fr['nenv'], fr.get('borders', ())))
            else:
                # refused while reading (bad border order, out-of-range value): the coded
                # count (t09, t11, t20, t21)
                # count (t09, t11, t20, t21); a refused num_env 0 frame counts 0 (t31)
                n = fr['nenv']
            self.n_before, self.n_last = self.n_last, n
        self.last_iid = self.last_icc = None   # values reset to index 0 (t12, t17)
        self.active = False                     # header-less frames and frames without PS
        return FrameOut(False, reason='error')  # are ignored until the next header (t18)

    def _read_only(self, fr):
        """A header-less frame read while the PS is off after an error."""
        nenv, fc = fr['nenv'], fr.get('frame_class', 0)
        bord = borders(fc, nenv, fr.get('borders', ()))
        if any(b < a for a, b in zip(bord, bord[1:])):
            self._error(fr)
            return
        im, cm = self.modes['iid_mode'], self.modes['icc_mode']
        iid = icc = [None]
        if nenv > 0:
            if im is not None:
                lim = 15 if im >= 3 else 7
                iid = self._resolve(fr['iid'], self.last_iid, -lim, lim)
            if cm is not None:
                icc = self._resolve(fr['icc'], self.last_icc, 0, 7)
            if iid is None or icc is None:
                self._error(fr)
                return
            self.last_iid, self.last_icc = _keep(self.last_iid, iid[-1]), _keep(self.last_icc, icc[-1])
        self.n_before, self.n_last = self.n_last, len(bord)
        if self.ro_sets_cfg and fr.get('cfg') is not None:
            self._set_cfg(fr['cfg'])      # read to its end: counts for the configuration

    def _fit(self, v, n, n0=False):
        """Kept values v (another band count) as n values (t29)."""
        v = np.asarray(v)
        if len(v) == n or (n0 and self.band_change == 'keep'):
            return v
        if self.band_change == 'resample':
            return v[np.arange(n) * len(v) // n]
        return np.concatenate([v, np.zeros(max(0, n - len(v)), int)])[:n]

    def _resolve(self, coded, ref, lo, hi):
        vals = []
        for e, (dt, d) in enumerate(coded):
            if dt:
                base = ref if e == 0 else vals[-1]
                base = np.zeros(len(d), int) if base is None else self._fit(base, len(d))
                v = base + np.asarray(d)
            else:
                v = np.cumsum(d)
            if v.min() < lo or v.max() > hi:
                return None
            vals.append(v)
        return vals

    # -- one frame ---------------------------------------------------------------------
    def frame(self, fr):
        if not fr.get('ps'):
            if not self.active or self.last is None:
                return FrameOut(False, reason='no PS')
            bord, targets, cfg = self.last
            changed, old = self._changed()
            start = self._start(cfg, old if changed else None)
            self._put(-1, start, cfg)
            H = interpolate(start, targets, bord)
            return FrameOut(True, H, cfg, 'repeat', start, changed)
        err = fr.get('error')
        if err == 'header':
            self.hdr_seen = True
            return self._error(None)
        if err is not None and not fr.get('iid') and not fr.get('icc'):
            return self._error(fr)
        if err is not None:
            # a length error found after reading the frame: errors met while reading come
            # first (they leave the coded envelope count)
            if fr.get('header') is not None:
                self.modes = dict(fr['header'])
            bord = borders(fr.get('frame_class', 0), fr['nenv'], fr.get('borders', ()))
            bad = any(b < a for a, b in zip(bord, bord[1:]))
            im, cm = self.modes['iid_mode'], self.modes['icc_mode']
            if not bad and fr['nenv'] > 0:
                lim = 15 if (im or 0) >= 3 else 7
                bad = ((im is not None and self._resolve(fr['iid'], self.last_iid, -lim, lim) is None) or
                       (cm is not None and self._resolve(fr['icc'], self.last_icc, 0, 7) is None))
            if bad:
                fr = dict(fr, error='value')
            return self._error(fr)
        if fr.get('header') is None and not self.active:
            if self.hdr_seen:
                # after an error: read but not applied (mono, no stored targets); its grid
                # and its values still count (verify seeds 1003, 1006)
                self._read_only(fr)
            # before any header the PS extension is skipped as unknown (t18)
            return FrameOut(False, reason='no header')
        if fr.get('header') is not None:
            self.hdr_seen = True
        h = fr.get('header')
        if h is not None:
            self.modes = dict(h)
        nenv, fc = fr['nenv'], fr.get('frame_class', 0)
        bord = borders(fc, nenv, fr.get('borders', ()))
        if any(b < a for a, b in zip(bord, bord[1:])):
            return self._error(fr)
        cfg = fr['cfg'] or self.hdr_cfg or 20   # None: a header without IID and ICC keeps the configuration (t27)
        im, cm = self.modes['iid_mode'], self.modes['icc_mode']
        if im is None:
            self.last_iid = None   # disabled: index 0 (t04, t07)
        if cm is None:
            self.last_icc = None
        if nenv == 0:
            iid = [None if self.last_iid is None or im is None else self._fit(self.last_iid, NBANDS[im], True)]
            icc = [None if self.last_icc is None or cm is None else self._fit(self.last_icc, NBANDS[cm], True)]
            lim = 15 if (im or 0) >= 3 else 7
            if im is not None and iid[0] is not None and np.abs(iid[0]).max() > lim:
                return self._error(fr)    # kept fine-grid values on the coarse grid (t22)
        else:
            iid = [None] * nenv
            icc = [None] * nenv
            if im is not None:
                lim = 15 if im >= 3 else 7
                iid = self._resolve(fr['iid'], self.last_iid, -lim, lim)
                if iid is None:
                    return self._error(fr)
            if cm is not None:
                icc = self._resolve(fr['icc'], self.last_icc, 0, 7)
                if icc is None:
                    return self._error(fr)
        if len(bord) > len(iid):           # the extra envelope of frame_class 1
            iid, icc = iid + [iid[-1]], icc + [icc[-1]]
        self.n_before, self.n_last = self.n_last, len(bord)
        changed, old = self._set_cfg(cfg)
        start = self._start(cfg, old if changed else None)
        targets = [self.matrices(a, c, im, cm, cfg) for a, c in zip(iid, icc)]
        H = interpolate(start, targets, bord)
        self._put(-1, start, cfg)
        for e, T in enumerate(targets):
            self._put(e, T, cfg)
        self.last_iid, self.last_icc = _keep(self.last_iid, iid[-1]), _keep(self.last_icc, icc[-1])
        self.last = (bord, targets, cfg)
        self.active = True
        return FrameOut(True, H, cfg, 'ps', start, changed)
