#!/usr/bin/env python3
"""Black-box experiment helper: rewrite bsid (and optionally fscod) of every AC-3 syncframe.

usage: patch_bsid.py <in.ac3> <out.ac3> <bsid> [fscod|-] [eac3_bsid]

Walks the elementary stream by the A/52:2018 Table 5.18 frame size of the *input* header,
rewrites the 5-bit bsid field (byte 5, bits 7..3) and, when given, the 2-bit fscod field, then
recomputes crc1 (first 5/8 of the frame) and crc2 (whole frame). E-AC-3 frames (bsid > 10) are
copied unchanged unless eac3_bsid is given (then only crc2 is recomputed); a truncated final frame
is always copied unchanged. Used to build the bsid 9/10 vectors in STATUS.md.
"""
import sys

POLY = 0x8005
BITRATE = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640]
W44 = [69, 87, 104, 121, 139, 174, 208, 243, 278, 348, 417, 487, 557, 696, 835, 975, 1114, 1253,
       1393]


def crc16(data, crc=0):
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def solve_crc1(frame, five8):
    """crc1 sits at the start of the protected span, so solve the 16 linear equations."""
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


def frame_words(fscod, frmsizecod):
    i = frmsizecod >> 1
    if fscod == 0:
        return 2 * BITRATE[i]
    if fscod == 1:
        return W44[i] + (frmsizecod & 1)
    return 3 * BITRATE[i]


def main():
    data = bytearray(open(sys.argv[1], "rb").read())
    bsid = int(sys.argv[3])
    fscod = int(sys.argv[4]) if len(sys.argv) > 4 and sys.argv[4] != "-" else None
    eac3_bsid = int(sys.argv[5]) if len(sys.argv) > 5 else None
    o = 0
    count = 0
    while o + 8 <= len(data):
        assert data[o] == 0x0B and data[o + 1] == 0x77, "lost sync at %d" % o
        if data[o + 5] >> 3 > 10:
            # E-AC-3 (Annex E): bsid sits in the same byte; only crc2 (whole frame) protects it.
            n = ((((data[o + 2] & 7) << 8) | data[o + 3]) + 1) * 2
            if o + n > len(data):
                break
            if eac3_bsid is not None:
                fr = data[o:o + n]
                fr[5] = (fr[5] & 7) | (eac3_bsid << 3)
                fr[n - 2:n] = crc16(bytes(fr[2:n - 2])).to_bytes(2, "big")
                assert crc16(bytes(fr[2:])) == 0
                data[o:o + n] = fr
                count += 1
            o += n
            continue
        n = frame_words(data[o + 4] >> 6, data[o + 4] & 0x3F) * 2
        if o + n > len(data):
            break
        fr = data[o:o + n]
        fr[5] = (fr[5] & 7) | (bsid << 3)
        if fscod is not None:
            fr[4] = (fr[4] & 0x3F) | (fscod << 6)
        words = n // 2
        five8 = ((words >> 1) + (words >> 3)) * 2
        fr[2:4] = solve_crc1(fr, five8).to_bytes(2, "big")
        fr[n - 2:n] = crc16(bytes(fr[2:n - 2])).to_bytes(2, "big")
        assert crc16(bytes(fr[2:five8])) == 0 and crc16(bytes(fr[2:])) == 0
        data[o:o + n] = fr
        o += n
        count += 1
    open(sys.argv[2], "wb").write(data)
    print("patched %d syncframes" % count, file=sys.stderr)


main()
