#!/usr/bin/env python3
"""Black-box experiment helper: patch single bits of AC-3 syncframes and fix CRCs.

usage: patch_bits.py <trace.jsonl> <in.ac3> <out.ac3> <au>:<blk>:<ch>:<field>=<0|1> ...
field: blksw | dithflag. Bit positions come from `examples/trace.rs` (our own parser).
Only AC-3 (non E-AC-3) syncframes are supported (crc1 and crc2 are recomputed).
"""
import json
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


def main():
    traces = [json.loads(l) for l in open(sys.argv[1])]
    data = bytearray(open(sys.argv[2], "rb").read())
    by_au = {}
    for t in traces:
        by_au.setdefault(t["au"], []).append(t)
    touched = set()
    for spec in sys.argv[4:]:
        loc, val = spec.split("=")
        au, blk, ch, field = loc.split(":")
        t = by_au[int(au)][0]
        assert not t["eac3"]
        blk, ch, val = int(blk), int(ch), int(val)
        if field == "dithflag":
            bit = t["dithflag_bits"][blk * t["nfchans"] + ch]
        elif field == "blksw":
            bit = t["insert_bits"][blk] - t["nfchans"] + ch
        else:
            raise SystemExit("field?")
        byte = t["offset"] + bit // 8
        mask = 0x80 >> (bit % 8)
        data[byte] = (data[byte] | mask) if val else (data[byte] & ~mask)
        touched.add(int(au))
    for au in touched:
        t = by_au[au][0]
        o, n = t["offset"], t["frame_bytes"]
        fr = data[o:o + n]
        words = n // 2
        five8 = ((words >> 1) + (words >> 3)) * 2
        c1 = solve_crc1(fr, five8)
        fr[2:4] = c1.to_bytes(2, "big")
        fr[n - 2:n] = crc16(bytes(fr[2:n - 2])).to_bytes(2, "big")
        assert crc16(bytes(fr[2:five8])) == 0 and crc16(bytes(fr[2:])) == 0
        data[o:o + n] = fr
    open(sys.argv[3], "wb").write(data)


main()
