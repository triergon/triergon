#!/usr/bin/env python3
"""Write Layer I and Layer II streams of random, syntactically valid frames.

usage: random_frames.py OUTPUT_DIRECTORY

FFmpeg has no Layer I encoder and its Layer II encoder uses neither joint stereo nor
every allocation table, so these streams cover what the encoded vectors cannot: every
allocation, scalefactor selection, grouped quantizer, joint stereo bound, CRC word and
the MPEG-2 low sampling frequencies. Allocations, scalefactors (indices 8 to 40, so
the output stays within full scale) and sample codes are drawn from a fixed seed.
The frame syntax follows ISO/IEC 11172-3 2.4.1.5 (Layer I) and 2.4.1.6 (Layer II).
"""
import os
import random
import sys

RATES = {1: [44100, 48000, 32000], 2: [22050, 24000, 16000]}
BITRATES = {
    (1, 1): [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
    (1, 2): [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
    (2, 1): [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
    (2, 2): [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
}
WIDE_LOW = [3, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535]
WIDE_MID = [3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 65535]
WIDE_HIGH = [3, 5, 7, 9, 15, 31, 65535]
WIDE_TOP = [3, 5, 65535]
NARROW_LOW = [3, 5, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767]
NARROW_HIGH = [3, 5, 9, 15, 31, 63, 127]
LSF_LOW = [3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383]
TABLES = {
    "a": [(3, 4, WIDE_LOW), (8, 4, WIDE_MID), (12, 3, WIDE_HIGH), (4, 2, WIDE_TOP)],
    "b": [(3, 4, WIDE_LOW), (8, 4, WIDE_MID), (12, 3, WIDE_HIGH), (7, 2, WIDE_TOP)],
    "c": [(2, 4, NARROW_LOW), (6, 3, NARROW_HIGH)],
    "d": [(2, 4, NARROW_LOW), (10, 3, NARROW_HIGH)],
    "lsf": [(4, 4, LSF_LOW), (7, 3, NARROW_HIGH), (19, 2, [3, 5, 9])],
}
GROUPED_BITS = {3: 5, 5: 7, 9: 10}


class Bits:
    def __init__(self):
        self.bits = []

    def put(self, value, count):
        self.bits.extend((value >> i) & 1 for i in range(count - 1, -1, -1))

    def tobytes(self, length):
        out = bytearray(length)
        for i, bit in enumerate(self.bits[: length * 8]):
            out[i // 8] |= bit << (7 - i % 8)
        return bytes(out)


def crc16(data, bits):
    crc = 0xFFFF
    for i in range(bits):
        bit = (data[i // 8] >> (7 - i % 8)) & 1
        crc = ((crc << 1) & 0xFFFF) ^ (0x8005 if ((crc >> 15) ^ bit) else 0)
    return crc


def header(version, layer, bitrate_index, rate_index, padding, mode, extension, crc):
    word = 0xFFE00000 | ((3 if version == 1 else 2) << 19) | ((4 - layer) << 17)
    word |= (0 if crc else 1) << 16 | bitrate_index << 12 | rate_index << 10 | padding << 9
    return word | mode << 6 | extension << 4


def frame_bytes(version, layer, bitrate, rate, padding):
    if layer == 1:
        return (12 * bitrate * 1000 // rate + padding) * 4
    return 144 * bitrate * 1000 // rate + padding


def layer2_table(version, bitrate, rate, channels):
    if version == 2:
        return TABLES["lsf"]
    per_channel = bitrate // channels
    if per_channel <= 48:
        return TABLES["d" if rate == 32000 else "c"]
    if per_channel <= 80 or rate == 48000:
        return TABLES["a"]
    return TABLES["b"]


def frame(rng, version, layer, bitrate_index, rate_index, mode, extension, crc):
    rate = RATES[version][rate_index]
    bitrate = BITRATES[(version, layer)][bitrate_index]
    length = frame_bytes(version, layer, bitrate, rate, 0)
    channels = 1 if mode == 3 else 2
    if layer == 1:
        rows = [(4, None)] * 32
    else:
        rows = [(bits, q) for count, bits, q in layer2_table(version, bitrate, rate, channels) for _ in range(count)]
    sblimit = len(rows)
    bound = min(4 * (extension + 1), sblimit) if mode == 1 else sblimit
    density = rng.choice([0.3, 0.6, 0.9])
    while True:
        alloc = [[0] * sblimit for _ in range(2)]
        for sb, (bits, q) in enumerate(rows):
            for ch in range(channels if sb < bound else 1):
                if rng.random() < density:
                    top = 14 if layer == 1 else len(q)
                    alloc[ch][sb] = rng.randint(1, min(top, max(1, int(top * rng.random() ** 2) + 1)))
            if sb >= bound:
                alloc[1][sb] = alloc[0][sb]
        used = 32 + (16 if crc else 0)
        for sb, (bits, q) in enumerate(rows):
            used += bits * (channels if sb < bound else 1)
        for sb, (bits, q) in enumerate(rows):
            for ch in range(channels):
                a = alloc[ch][sb]
                if not a:
                    continue
                used += 6 if layer == 1 else 2 + 18
                if ch < (channels if sb < bound else 1):
                    if layer == 1:
                        used += 12 * (a + 1)
                    else:
                        levels = q[a - 1]
                        used += 12 * (GROUPED_BITS[levels] if levels in GROUPED_BITS else 3 * levels.bit_length())
        if used <= length * 8:
            break
        density *= 0.8
    w = Bits()
    w.put(header(version, layer, bitrate_index, rate_index, 0, mode, extension, crc), 32)
    if crc:
        w.put(0, 16)
    protected_start = len(w.bits)
    for sb, (bits, q) in enumerate(rows):
        for ch in range(channels if sb < bound else 1):
            w.put(alloc[ch][sb], bits)
    selections = [[0] * sblimit for _ in range(2)]
    if layer == 2:
        for sb in range(sblimit):
            for ch in range(channels):
                if alloc[ch][sb]:
                    selections[ch][sb] = rng.randint(0, 3)
                    w.put(selections[ch][sb], 2)
    protected_bits = len(w.bits) - protected_start
    for sb in range(sblimit):
        for ch in range(channels):
            if alloc[ch][sb]:
                count = 1 if layer == 1 else [3, 2, 1, 2][selections[ch][sb]]
                for _ in range(count):
                    w.put(rng.randint(8, 40), 6)
    for granule in range(12):
        for sb in range(sblimit):
            for ch in range(channels if sb < bound else 1):
                a = alloc[ch][sb]
                if not a:
                    continue
                if layer == 1:
                    w.put(rng.randint(0, (1 << (a + 1)) - 2), a + 1)
                    continue
                levels = rows[sb][1][a - 1]
                if levels in GROUPED_BITS:
                    w.put(rng.randint(0, levels ** 3 - 1), GROUPED_BITS[levels])
                else:
                    for _ in range(3):
                        w.put(rng.randint(0, levels - 1), levels.bit_length())
    data = bytearray(w.tobytes(length))
    if crc:
        body = bytes(data[2:4]) + bytes(data[6:])
        value = crc16(body, 16 + protected_bits)
        data[4:6] = value.to_bytes(2, "big")
    return bytes(data)


def stream(path, seed, frames, version, layer, bitrate_index, rate_index, modes, crc=False):
    rng = random.Random(seed)
    with open(path, "wb") as f:
        for i in range(frames):
            mode, extension = modes[i % len(modes)]
            f.write(frame(rng, version, layer, bitrate_index, rate_index, mode, extension, crc))


def main():
    out = sys.argv[1]
    joint = [(1, 0), (1, 1), (1, 2), (1, 3)]
    stream(os.path.join(out, "l1_44k_384_stereo.mp1"), 1, 120, 1, 1, 12, 0, [(0, 0)])
    stream(os.path.join(out, "l1_48k_256_joint.mp1"), 2, 120, 1, 1, 8, 1, joint)
    stream(os.path.join(out, "l1_32k_96_mono.mp1"), 3, 120, 1, 1, 3, 2, [(3, 0)])
    stream(os.path.join(out, "l1_44k_448_dual_crc.mp1"), 4, 120, 1, 1, 14, 0, [(2, 0)], crc=True)
    stream(os.path.join(out, "l1_24k_128_joint.mp1"), 5, 120, 2, 1, 8, 1, joint)
    stream(os.path.join(out, "l2r_48k_384_joint.mp2"), 6, 60, 1, 2, 14, 1, joint)
    stream(os.path.join(out, "l2r_44k_112_stereo.mp2"), 7, 60, 1, 2, 7, 0, [(0, 0)])
    stream(os.path.join(out, "l2r_44k_160_stereo_crc.mp2"), 8, 60, 1, 2, 10, 0, [(0, 0)], crc=True)
    stream(os.path.join(out, "l2r_44k_64_joint.mp2"), 9, 60, 1, 2, 4, 0, joint)
    stream(os.path.join(out, "l2r_32k_48_mono.mp2"), 10, 60, 1, 2, 3, 2, [(3, 0)])
    stream(os.path.join(out, "l2r_32k_80_mono.mp2"), 11, 60, 1, 2, 6, 2, [(3, 0)])
    stream(os.path.join(out, "l2r_16k_64_joint.mp2"), 12, 60, 2, 2, 8, 2, joint)
    stream(os.path.join(out, "l2r_22k_160_dual.mp2"), 13, 60, 2, 2, 14, 0, [(2, 0)])


if __name__ == "__main__":
    main()
