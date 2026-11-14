"""o20 (case 2): IPD/OPD across a 34 -> 20 change after IID last ran 34 bands. Builds the
open stream (4 frames IID mode 2 / ICC mode 2 with IPD/OPD, then 6 frames IID disabled /
ICC mode 1 with IPD/OPD, whose 17 coded values IID's last band count gives) and variants
of it, decodes them with the reference and with one or more builds of this decoder
(DECODE_AAC, or the binaries given), and prints per-frame residuals and a per-QMF-band
look at the frames that differ.

usage: o20_ipd_change.py OUTDIR [binary ...]"""
import hashlib

from olib import *

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
BINS = sys.argv[2:] or [DECODE]
OPEN = os.path.join(ROOT, 'harness', 'codecs', 'aac', 'ps', 'open')

IPD = [1, 2, 3, 4, 5, 6, 7, 1, 2, 3, 4, 5, 6, 7, 1, 2, 3]
OPD = [2, 1, 0, 7, 6, 5, 4, 3, 2, 1, 0, 7, 6, 5, 4, 3, 2]


def open_stream(path):
    ipd = ipdopd_ext(IPD, OPD)
    iid34 = [((b * 3) % 11) - 5 for b in range(34)]
    icc20 = [(b * 5) % 8 for b in range(20)]
    f34 = payload(iid=iid34, iid_mode=2, icc=[2] * 34, icc_mode=2, ext=ipd)
    f20 = payload(iid=None, icc=icc20, icc_mode=1, ext=ipd)
    write(path, [f34] * 4 + [f20] * 6, seed=11)


def look(d, f):
    Dq = psana.qmf_bands(np.concatenate([np.zeros(640), d]))
    s = slice(32 * f + 10, 32 * f + 42)
    return np.sqrt((np.abs(Dq[s]) ** 2).mean(0)) / 32768


def compare(path, label, bins=BINS, bands=True):
    y, msgs, _ = decode_log(path)
    print('== %s %s' % (label, '; '.join(msgs)))
    for b in bins:
        z = rust(path, b)
        r = per_frame(z, y).max(1)
        print('   %-12s %s' % (os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(b)))) or b, fmt(r)))
        if bands:
            for f in range(len(r)):
                if r[f] > 5e-8:
                    eb = look(z[:, 0] - y[:len(z), 0], f)
                    top = np.argsort(eb)[::-1][:8]
                    print('      f%d L worst QMF: %s' % (f, ' '.join('%d:%.1e' % (k, eb[k]) for k in sorted(top))))


if __name__ == '__main__':
    p = os.path.join(out, 'ipd_after_iid34.aac')
    open_stream(p)
    same = hashlib.sha256(open(p, 'rb').read()).hexdigest() == hashlib.sha256(
        open(os.path.join(OPEN, 'ps-open-ipd-after-iid34.adts'), 'rb').read()).hexdigest()
    print('open stream rebuilt identical:', same)
    compare(p, 'open stream')
