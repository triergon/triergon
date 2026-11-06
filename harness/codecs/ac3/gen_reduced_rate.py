#!/usr/bin/env python3
"""Self-authored AC-3 probe streams for the bit allocation of bsid 9/10 (reduced sample rate).

usage: gen_reduced_rate.py <out.ac3> <bsid> <fscod> <frames> <seed> [acmod]

Every syncframe is 1/0 + LFE (acmod 1, default) or 2/0 + LFE with coupling and rematrixing
(acmod 2), frmsizecod 37, written bit by bit from the A/52:2018 5.4 syntax: block 0 carries random
D15 exponents (random chbwcod / coupling range), random coupling coordinates, bit-allocation
parameters, SNR offsets and coupling leaks, and dithflag = 0; blocks 1..5 reuse everything
(all-zero header bits). All mantissa
bits are zero, so each bin decodes to the code-0 level of its bap (-2/3, -4/5, ... for bap 1..5,
0 for bap >= 6, 0 for bap 0 without dither): the decoded output exposes the per-bin bap, hence
the hearing-threshold table a decoder uses. No encoder or FFmpeg code is involved; the stream
is parseable under any bit allocation because nothing follows the mantissas but zero padding.
"""
import random
import sys

POLY = 0x8005


def crc16(data, crc=0):
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def solve_crc1(frame, five8):
    n = five8 - 4
    target = crc16(bytes(2) + bytes(frame[4:five8]))
    cols = [crc16(bytes([(1 << i) >> 8, (1 << i) & 0xFF]) + bytes(n)) for i in range(16)]
    rows = []
    for r in range(16):
        lhs = 0
        for i, c in enumerate(cols):
            if c >> r & 1:
                lhs |= 1 << i
        rows.append([lhs, target >> r & 1])
    piv = []
    r = 0
    for col in range(16):
        p = next((k for k in range(r, 16) if rows[k][0] >> col & 1), None)
        if p is None:
            continue
        rows[r], rows[p] = rows[p], rows[r]
        for k in range(16):
            if k != r and rows[k][0] >> col & 1:
                rows[k][0] ^= rows[r][0]
                rows[k][1] ^= rows[r][1]
        piv.append((col, r))
        r += 1
    x = 0
    for col, row in piv:
        if rows[row][1]:
            x |= 1 << col
    return x


class W:
    def __init__(self):
        self.bits = []

    def put(self, v, n):
        for k in range(n - 1, -1, -1):
            self.bits.append((v >> k) & 1)

    def bytes(self, n):
        out = bytearray(n)
        assert len(self.bits) <= 8 * n
        for i, b in enumerate(self.bits):
            if b:
                out[i // 8] |= 0x80 >> (i % 8)
        return out


def exps(w, rng, count, cpl=False):
    """D15: 4-bit absolute exponent then 7-bit groups of three differentials (5.4.3.16).
    Coupling: the absolute value (<< 1) is only the reference; count bins are all grouped."""
    e = rng.randint(0, 12 if cpl else 15)
    w.put(e, 4)
    if cpl:
        e <<= 1  # reference value; must stay <= 24 (5.4.3.16)
    for _ in range(count // 3 if cpl else (count - 1) // 3):
        code = 0
        for _ in range(3):
            d = rng.randint(-2, 2)
            if not 0 <= e + d <= 24:
                d = -d
            e += d
            code = code * 5 + d + 2
        w.put(code, 7)


def frame(rng, bsid, fscod, acmod):
    words = {0: 1280, 1: 1394, 2: 1920}[fscod]  # Table 5.18, frmsizecod 37
    w = W()
    w.put(0x0B77, 16)
    w.put(0, 16)  # crc1
    w.put(fscod, 2)
    w.put(37, 6)
    w.put(bsid, 5)
    w.put(0, 3)  # bsmod
    w.put(acmod, 3)
    if acmod == 2:
        w.put(0, 2)  # dsurmod
    w.put(1, 1)  # lfeon
    w.put(27, 5)  # dialnorm
    w.put(0, 3)  # compre, langcode, audprodie
    w.put(0, 2)  # copyrightb, origbs
    w.put(0, 3)  # timecod1e, timecod2e, addbsie
    nfch = 2 if acmod == 2 else 1
    # block 0
    w.put(0, nfch)  # blksw
    w.put(0, nfch)  # dithflag
    w.put(0, 1)  # dynrnge
    w.put(1, 1)  # cplstre
    if acmod == 2:
        w.put(1, 1)  # cplinu
        w.put(3, 2)  # chincpl[0], chincpl[1]
        w.put(0, 1)  # phsflginu
        cplbegf = rng.randint(0, 12)
        cplendf = rng.randint(max(cplbegf - 2, 0), 15)
        w.put(cplbegf, 4)
        w.put(cplendf, 4)
        nsub = cplendf + 3 - cplbegf
        strc = [rng.randint(0, 1) for _ in range(nsub - 1)]
        for b in strc:
            w.put(b, 1)  # cplbndstrc
        ncplbnd = nsub - sum(strc)
        for _ in range(2):
            w.put(1, 1)  # cplcoe
            w.put(rng.randint(0, 3), 2)  # mstrcplco
            for _ in range(ncplbnd):
                w.put(rng.randint(0, 15), 4)  # cplcoexp
                w.put(rng.randint(0, 15), 4)  # cplcomant
        w.put(1, 1)  # rematstr
        w.put(rng.getrandbits(4), 2 if cplbegf == 0 else 3 if cplbegf < 3 else 4)  # rematflg
        w.put(1, 2)  # cplexpstr D15
    else:
        w.put(0, 1)  # cplinu
    w.put(0x5 if acmod == 2 else 1, 2 * nfch)  # chexpstr D15
    w.put(1, 1)  # lfeexpstr
    if acmod == 2:
        strt, end = 37 + 12 * cplbegf, 37 + 12 * (cplendf + 3)
        exps(w, rng, end - strt, cpl=True)
        for _ in range(2):
            exps(w, rng, strt)
            w.put(rng.randint(0, 3), 2)  # gainrng
    else:
        chbwcod = rng.choice([60, 60, rng.randint(0, 60)])
        w.put(chbwcod, 6)
        exps(w, rng, 73 + 3 * chbwcod)
        w.put(rng.randint(0, 3), 2)  # gainrng
    exps(w, rng, 7)  # LFE: absolute + 2 groups
    w.put(1, 1)  # baie
    w.put(rng.randint(0, 3), 2)  # sdcycod
    w.put(rng.randint(0, 3), 2)  # fdcycod
    w.put(rng.randint(0, 3), 2)  # sgaincod
    w.put(rng.randint(0, 3), 2)  # dbpbcod
    w.put(rng.randint(0, 7), 3)  # floorcod
    w.put(1, 1)  # snroffste
    w.put(rng.randint(5, 30), 6)  # csnroffst
    for _ in range(nfch + (acmod == 2) + 1):  # [cpl], fbw..., lfe
        w.put(rng.randint(0, 15), 4)  # fsnroffst
        w.put(rng.randint(0, 7), 3)  # fgaincod
    if acmod == 2:
        w.put(1, 1)  # cplleake
        w.put(rng.randint(0, 7), 3)  # cplfleak
        w.put(rng.randint(0, 7), 3)  # cplsleak
    w.put(0, 1)  # deltbaie
    w.put(0, 1)  # skiple
    # mantissas of block 0 and blocks 1..5 (every header bit 0 = reuse): all zero.
    f = w.bytes(words * 2)
    five8 = ((words >> 1) + (words >> 3)) * 2
    f[2:4] = solve_crc1(f, five8).to_bytes(2, "big")
    f[-2:] = crc16(bytes(f[2:-2])).to_bytes(2, "big")
    assert crc16(bytes(f[2:five8])) == 0 and crc16(bytes(f[2:])) == 0
    return f


def main():
    out, bsid, fscod, n, seed = sys.argv[1], *map(int, sys.argv[2:6])
    acmod = int(sys.argv[6]) if len(sys.argv) > 6 else 1
    rng = random.Random(seed)
    with open(out, "wb") as fh:
        for _ in range(n):
            fh.write(frame(rng, bsid, fscod, acmod))


main()
