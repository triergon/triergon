#!/usr/bin/env python3
"""Black-box experiment helper: insert/flip bits in E-AC-3 syncframes (positions from
examples/trace.rs, i.e. our own parser), drop the same number of unused bits after the
audio data, and recompute the frame CRC. Used to check FFmpeg's handling of syntax our
vectors do not exercise (phase flags, delta bit allocation, TPNP).

usage: insert_bits.py <trace.jsonl> <in> <out> <experiment> <au_from> <au_to>
experiment: phase | dba
"""
import json
import os
import sys

POLY = 0x8005


def crc16(data, crc=0):
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def bits_of(v, n):
    return [(v >> (n - 1 - k)) & 1 for k in range(n)]


def patch(fr, t, flips, inserts):
    n = t['frame_bytes']
    bits = [(fr[i // 8] >> (7 - i % 8)) & 1 for i in range(n * 8)]
    for p, v in flips:
        bits[p] = v
    k = sum(len(b) for _, b in inserts)
    end = t['audio_end_bit']
    if n * 8 - 18 - end < k:
        return None
    ins = dict(inserts)
    nb = []
    for i, b in enumerate(bits):
        if i in ins:
            nb.extend(ins[i])
        if end <= i < end + k:
            continue
        nb.append(b)
    assert len(nb) == len(bits)
    out = bytearray(sum(b << (7 - j) for j, b in enumerate(nb[i:i + 8])) for i in range(0, len(nb), 8))
    out[-2:] = crc16(out[2:-2]).to_bytes(2, 'big')
    return out


def main():
    tr = [json.loads(l) for l in open(sys.argv[1])]
    data = open(sys.argv[2], 'rb').read()
    exp, a, b = sys.argv[4], int(sys.argv[5]), int(sys.argv[6])
    out = bytearray()
    pos = 0
    done = 0
    for t in tr:
        o, n = t['offset'], t['frame_bytes']
        out += data[pos:o]
        fr = data[o:o + n]
        if t['eac3'] and a <= t['au'] <= b:
            blocks = t['blocks']
            flips, inserts = [], []
            if exp == 'phase':
                cpl_blocks = [x for x in blocks if x['cplstre'] and x['phsflginu_bit'] >= 0]
                if cpl_blocks:
                    flips.append((cpl_blocks[0]['phsflginu_bit'], 1))
                    active = False
                    for x in blocks:
                        if x['cplstre']:
                            active = x is cpl_blocks[0]
                        if active and x['cplcoe_any']:
                            pat = [(x['blk'] + j) % 2 for j in range(x['ncplbnd'])]
                            inserts.append((x['phsflg_bit'], pat))
            elif exp == 'dba':
                flips.append((t['dbaflde_bit'], 1))
                for x in blocks:
                    if x['blk'] < t['num_blocks'] - 1:
                        inserts.append((x['dba_bit'], [0]))
                    else:
                        # DBA=cplmode,cploff,cpllen,cplba,ch0mode,ch0off,ch0len,ch0ba (env)
                        cm, co, cl, cb, m0, o0, l0, b0 = [int(v) for v in os.environ.get(
                            'DBA', '1,31,3,7,1,10,4,6').split(',')]
                        seq = [1] + bits_of(cm, 2) + bits_of(m0, 2) + bits_of(0, 2)
                        if cm == 1:
                            seq += bits_of(0, 3) + bits_of(co, 5) + bits_of(cl, 4) + bits_of(cb, 3)
                        if m0 == 1:
                            seq += bits_of(0, 3) + bits_of(o0, 5) + bits_of(l0, 4) + bits_of(b0, 3)
                        inserts.append((x['dba_bit'], seq))
            p = patch(fr, t, flips, inserts) if (flips or inserts) else None
            if p is not None:
                fr = bytes(p)
                done += 1
        out += fr
        pos = o + n
    out += data[pos:]
    open(sys.argv[3], 'wb').write(out)
    print('patched frames', done)


main()
