"""Black-box probe of FFmpeg's channel mapping for ADTS channel configurations and PCEs.

Each syntactic element carries one non-zero coefficient of a distinct amplitude
(element k -> quantised value k+1 in bin 0, codebook 11). The FFmpeg output channel
peaks reveal which element landed in which output channel.
usage: probe_layout.py CHCFG 'S,C,C,L' [pce-spec]
  element list letters: S=SCE C=CPE L=LFE; with CHCFG 0 a PCE spec 'f=S0,C0;s=;b=C1;l=0'
  (optionally with heights: 'f=S0,C0,C1^1' where ^1 = top, ^2 = bottom)
"""
import sys, os, subprocess
import numpy as np
exec(open(os.path.join(os.path.dirname(__file__), 'probe_cb.py')).read().split('def frame')[0])

def adtsh(payload, chcfg):
    n = len(payload) + 7
    h = BW(); h.put(0xfff, 12); h.put(0, 1); h.put(0, 2); h.put(1, 1); h.put(1, 2)
    h.put(4, 4); h.put(0, 1); h.put(chcfg, 3); h.put(0, 4); h.put(n, 13); h.put(0x7ff, 11); h.put(0, 2)
    return h.bytes() + payload

CB11 = None
def cb11_code(v):
    import re
    src = open(os.path.join(os.path.dirname(__file__), '../../../../crates/codec-aac/src/huffman_tables.rs')).read()
    m = re.search(r'static SPECTRUM_11: \[\(u32, u8\); \d+\] = \[(.*?)\];', src, re.S)
    t = [(int(a, 16), int(b)) for a, b in re.findall(r'\(0x([0-9a-f]+), (\d+)\)', m.group(1))]
    c, l = t[v * 17]  # (v, 0)
    c0, l0 = t[0]
    return format(c, '0%db' % l) + '0' + format(c0, '0%db' % l0)  # sign +, then pair (0,0)

def ics(w, amp):
    w.put(100, 8); w.put(0, 1); w.put(0, 2); w.put(0, 1)
    if amp is None:
        w.put(0, 6); w.put(0, 1); w.put(0, 3)
        return
    w.put(1, 6); w.put(0, 1); w.put(11, 4); w.put(1, 5); w.putbits('0')
    w.put(0, 3); w.putbits(cb11_code(amp))

def build(chcfg, elems, pce, amps):
    w = BW()
    if pce is not None:
        w.put(5, 3); w.put(0, 4); w.put(1, 2); w.put(4, 4)
        parts = dict(p.split('=') for p in pce.split(';'))
        def lst(k):
            return [x for x in parts.get(k, '').split(',') if x]
        f, s, b, l = lst('f'), lst('s'), lst('b'), lst('l')
        w.put(len(f), 4); w.put(len(s), 4); w.put(len(b), 4); w.put(len(l), 2); w.put(0, 3); w.put(0, 4)
        w.put(0, 1); w.put(0, 1); w.put(0, 1)
        heights = []
        for grp in (f, s, b):
            for e in grp:
                h = 0
                if '^' in e: e, h = e.split('^'); h = int(h)
                heights.append(h)
                w.put(1 if e[0] == 'C' else 0, 1); w.put(int(e[1:]), 4)
        for e in l: w.put(int(e), 4)
        while len(w.bits) % 8: w.bits.append(0)   # ADTS payload starts byte aligned
        if any(heights):
            hb = BW(); hb.put(0xac, 8)
            for h in heights: hb.put(h, 2)
            while len(hb.bits) % 8: hb.bits.append(0)
            # CRC8 (poly 0x97? unknown): FFmpeg may reject; try both by caller
            body = hb.bytes()
            crc = 0
            w.put(len(body) + 1, 8)
            for x in body: w.put(x, 8)
            w.put(crc, 8)
        else:
            w.put(0, 8)
    tags = {}
    for k, e in enumerate(elems):
        typ = {'S': 0, 'C': 1, 'L': 3}[e[0]]
        tag = int(e[1:]) if len(e) > 1 else tags.get(typ, 0)
        tags[typ] = tag + 1
        w.put(typ, 3); w.put(tag, 4)
        if typ == 1:
            w.put(0, 1)
            ics(w, amps[k][0] if amps else None); ics(w, amps[k][1] if amps else None)
        else:
            ics(w, amps[k][0] if amps else None)
    w.put(7, 3)
    return adtsh(w.bytes(), chcfg)

def main():
    chcfg = int(sys.argv[1]); elems = sys.argv[2].split(','); pce = sys.argv[3] if len(sys.argv) > 3 else None
    amps = []; a = 1
    for e in elems:
        if e[0] == 'C': amps.append((a, a + 1)); a += 2
        else: amps.append((a, None)); a += 1
    data = build(chcfg, elems, pce, amps) + build(chcfg, elems, pce, None) * 2
    fn = '/tmp/probe_layout.aac'; open(fn, 'wb').write(data)
    pr = subprocess.run(['/opt/homebrew/bin/ffprobe', '-v', 'error', '-f', 'aac', '-i', fn, '-show_entries',
                         'stream=channels,channel_layout', '-of', 'csv=p=0'], capture_output=True, text=True)
    out = subprocess.run(['/opt/homebrew/bin/ffmpeg', '-nostdin', '-v', 'error', '-f', 'aac', '-i', fn, '-f', 'f32le', '-'],
                         capture_output=True)
    lay = pr.stdout.strip(); ch = int(lay.split(',')[0]) if lay else 0
    y = np.frombuffer(out.stdout, dtype='<f4')
    if ch == 0 or len(y) == 0:
        print('layout', lay, 'no output', out.stderr.decode()[:300]); return
    y = y.reshape(-1, ch)
    peaks = np.abs(y).max(axis=0)
    unit = peaks[peaks > 0].min() if (peaks > 0).any() else 1
    # amplitude of element value v is v^(4/3) relative
    inv = {}
    for k, e in enumerate(elems):
        for c, v in enumerate(amps[k]):
            if v: inv[v] = '%s%d.%d' % (e[0], k, c)
    base = None
    res = []
    for c in range(ch):
        if peaks[c] == 0: res.append('-'); continue
        # find v with ratio
        best = min(inv, key=lambda v: abs(peaks[c] / peaks.max() - (v ** (4 / 3)) / (max(inv) ** (4 / 3))))
        res.append(inv[best])
    print('layout', lay, '| channel order ->', ' '.join(res), out.stderr.decode().strip()[:200])

main()
