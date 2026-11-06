"""Black-box probe of FFmpeg's AAC spectral codebook decoding (development tool).

Builds ADTS frames (mono LC 44.1 kHz, ONLY_LONG, max_sfb=1, section codebook CB for
sfb 0, scalefactor = global_gain = 100) whose only spectral data is one candidate codeword
(plus sign bits for unsigned books), each followed by an all-zero frame, decodes them with
the FFmpeg binary and recovers the four decoded coefficients of sfb 0 by least squares on
the windowed IMDCT basis. Prints, per candidate bit pattern, the tuple FFmpeg decoded (or
'ERR' when FFmpeg rejected the frame).

usage: probe_cb.py CB OUTDIR pattern [pattern ...]   (pattern = string of 0/1)
"""
import sys, subprocess, os
import numpy as np

class BW:
    def __init__(s): s.bits = []
    def put(s, v, n):
        for i in range(n - 1, -1, -1): s.bits.append((v >> i) & 1)
    def putbits(s, b): s.bits.extend(int(c) for c in b)
    def bytes(s):
        b = s.bits + [0] * ((8 - len(s.bits) % 8) % 8)
        return bytes(int(''.join(map(str, b[i:i+8])), 2) for i in range(0, len(b), 8))

def adts(payload):
    n = len(payload) + 7
    h = BW(); h.put(0xfff, 12); h.put(0, 1); h.put(0, 2); h.put(1, 1); h.put(1, 2)
    h.put(4, 4); h.put(0, 1); h.put(1, 3); h.put(0, 4); h.put(n, 13); h.put(0x7ff, 11); h.put(0, 2)
    return h.bytes() + payload

def frame(cb, pattern):
    w = BW()
    w.put(0, 3); w.put(0, 4)            # SCE tag 0
    w.put(100, 8)                       # global gain
    w.put(0, 1); w.put(0, 2); w.put(0, 1)  # ics_reserved, ONLY_LONG, sine
    if pattern is None:
        w.put(0, 6); w.put(0, 1)        # max_sfb 0, no predictor
    else:
        w.put(1, 6); w.put(0, 1)        # max_sfb 1
        w.put(cb, 4); w.put(1, 5)       # section: cb, len 1
        w.putbits('0')                  # sf dpcm 0 (index 60)
    w.put(0, 1); w.put(0, 1); w.put(0, 1)  # pulse, tns, gain control
    if pattern is not None:
        w.putbits(pattern)
    w.put(7, 3)                         # END
    return adts(w.bytes())

def basis():
    N = 2048; n = np.arange(N); n0 = (N / 2 + 1) / 2
    win = np.sin(np.pi / N * (n + 0.5))
    B = np.array([win * (2.0 / N / 32768.0) * np.cos(2 * np.pi / N * (n + n0) * (k + 0.5)) for k in range(4)])
    return B

def main():
    cb = int(sys.argv[1]); od = sys.argv[2]; pats = sys.argv[3:]
    os.makedirs(od, exist_ok=True)
    data = b''
    for p in pats:
        data += frame(cb, p) + frame(cb, None) + frame(cb, None)
    fn = os.path.join(od, 'probe_cb%d.aac' % cb)
    open(fn, 'wb').write(data)
    out = subprocess.run(['/opt/homebrew/bin/ffmpeg', '-nostdin', '-v', 'error', '-y', '-f', 'aac', '-i', fn,
                          '-f', 'f32le', '-c:a', 'pcm_f32le', '-'], capture_output=True)
    y = np.frombuffer(out.stdout, dtype='<f4')
    errs = out.stderr.decode().strip()
    B = basis()
    # Without errors each probe yields 3 frames (3072 samples). Error frames are dropped,
    # so walk frame by frame using the expected silence of the trailing frames.
    nprobe = len(pats)
    if len(y) == nprobe * 3072:
        for i, p in enumerate(pats):
            seg = y[i * 3072:i * 3072 + 2048]
            c, *_ = np.linalg.lstsq(B.T, seg, rcond=None)
            resid = np.abs(seg - B.T @ c).max()
            print(p, ' '.join('%7.3f' % v for v in c), 'resid %.1e' % resid)
    else:
        print('frames decoded', len(y) / 1024, 'expected', nprobe * 3, errs[:500])

main()
