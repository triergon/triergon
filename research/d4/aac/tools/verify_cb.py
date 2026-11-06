"""Verify the crate's spectral Huffman tables against the FFmpeg binary (black box).

For every symbol of SPECTRUM_<CB> in crates/codec-aac/src/huffman_tables.rs a probe frame
carrying exactly that codeword (sign bits positive, escape 16 when needed) is decoded by
FFmpeg; the recovered coefficients must equal the tuple the table assigns to the symbol.
usage: verify_cb.py CB [CB ...]
"""
import sys, re, os, subprocess
import numpy as np
sys.path.insert(0, os.path.dirname(__file__))
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '../../../..'))
src = open(os.path.join(ROOT, 'crates/codec-aac/src/huffman_tables.rs')).read()

def table(name):
    m = re.search(r'static %s: \[\(u32, u8\); \d+\] = \[(.*?)\];' % name, src, re.S)
    return [(int(a, 16), int(b)) for a, b in re.findall(r'\(0x([0-9a-f]+), (\d+)\)', m.group(1))]

exec(open(os.path.join(os.path.dirname(__file__), 'probe_cb.py')).read().split('def main')[0])

def vals(cb, i):
    if cb <= 4:
        off = 1 if cb <= 2 else 0
        return [i // 27 - off, (i // 9) % 3 - off, (i // 3) % 3 - off, i % 3 - off]
    mod, off = {5: (9, 4), 6: (9, 4), 7: (8, 0), 8: (8, 0), 9: (13, 0), 10: (13, 0), 11: (17, 0)}[cb]
    return [i // mod - off, i % mod - off]

def run(cb):
    t = table('SPECTRUM_%d' % cb)
    unsigned = cb in (3, 4, 7, 8, 9, 10, 11)
    zero_sym = {5: 40, 6: 40}.get(cb, 0)
    pats, exp = [], []
    for i, (c, l) in enumerate(t):
        v = vals(cb, i)
        p = format(c, '0%db' % l)
        if unsigned:
            p += ''.join('0' for x in v if x != 0)
        if cb == 11:
            for x in v:
                if x == 16: p += '0' + '0000'
        e = list(v)
        if cb >= 5:
            zc, zl = t[zero_sym]
            p += format(zc, '0%db' % zl)
            e += [0, 0]
        pats.append(p); exp.append(e)
    data = b''.join(frame(cb, p) + frame(cb, None) + frame(cb, None) for p in pats)
    fn = os.path.join(ROOT, 'target/agent-aac/probe/verify_cb%d.aac' % cb)
    os.makedirs(os.path.dirname(fn), exist_ok=True)
    open(fn, 'wb').write(data)
    out = subprocess.run(['/opt/homebrew/bin/ffmpeg', '-nostdin', '-v', 'error', '-y', '-f', 'aac', '-i', fn,
                          '-f', 'f32le', '-c:a', 'pcm_f32le', '-'], capture_output=True)
    y = np.frombuffer(out.stdout, dtype='<f4')
    if len(y) != len(pats) * 3072:
        print('cb', cb, 'decode count mismatch', len(y) / 1024, out.stderr.decode()[:300]); return
    B = basis(); bad = 0
    for i, p in enumerate(pats):
        seg = y[i * 3072:i * 3072 + 2048]
        c, *_ = np.linalg.lstsq(B.T, seg, rcond=None)
        q = [int(round(np.sign(x) * abs(x) ** 0.75)) for x in c]
        if q != exp[i]:
            bad += 1
            print('cb%d sym %d code %s: table %s ffmpeg %s' % (cb, i, p, exp[i], q))
    print('cb', cb, 'symbols', len(pats), 'mismatches', bad)

for a in sys.argv[1:]:
    run(int(a))
