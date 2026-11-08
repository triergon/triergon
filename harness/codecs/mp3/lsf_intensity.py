#!/usr/bin/env python3
"""Write MPEG-2 Layer III streams with intensity stereo, which no available encoder
produces.

usage: lsf_intensity.py OUTPUT_DIRECTORY

Each stream is 22.05 kHz joint stereo at 64 kbit/s: three frames whose left channel
carries a pattern of unit lines up to line 420 and whose right channel is zero above
line 9, so bands from 2 on are intensity coded with the right channel's scalefactors
as positions; then silent frames. The streams vary `intensity_scale`, mid/side and the
width of the position fields, including their largest value. Huffman codes are those of
ISO/IEC 11172-3 Table B.7, table 1; the syntax is ISO/IEC 13818-3 2.4.1.7.
"""
import os
import sys

FRAME = 208  # 72 * 64000 / 22050
SIDE = 17
# Table B.7, table 1: (|x|, |y|) -> (code, length).
TABLE_1 = {(0, 0): (1, 1), (0, 1): (1, 3), (1, 0): (1, 2), (1, 1): (0, 3)}


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


def spectrum(lines):
    """Huffman bits and big_values of `lines` (line -> -1, 0 or 1) with table 1."""
    w = Bits()
    pairs = max(lines) // 2 + 1 if lines else 0
    for p in range(pairs):
        x, y = lines.get(2 * p, 0), lines.get(2 * p + 1, 0)
        w.put(*TABLE_1[(abs(x), abs(y))])
        for v in (x, y):
            if v:
                w.put(1 if v < 0 else 0, 1)
    return w.bits, pairs


def frame(scale, mid_side, slen, signal):
    header = 0xFFF38050 | (0x20 if mid_side else 0)  # MPEG-2, 64 kbit/s, joint, intensity
    side, main = Bits(), Bits()
    side.put(0, 8)  # main_data_begin
    side.put(0, 2)  # private bits
    left = {l: (1 if (l // 7) % 2 else -1) for l in range(0, 420, 7)} if signal else {}
    right = {2: 1, 9: -1} if signal else {}
    for ch, lines in enumerate((left, right)):
        part2 = Bits()
        if ch == 1:
            # nr_of_sfb table 3: slen = (c / 36, c % 36 / 6, c % 6) over 7 + 7 + 7 bands.
            compress = 2 * (36 * slen + 6 * slen + slen) + scale
            for band in range(21):
                part2.put((band * 5 % 16) % (1 << slen) if signal else 0, slen)
        else:
            compress = 0
        huffman, pairs = spectrum(lines)
        bits = part2.bits + huffman
        side.put(len(bits), 12)
        side.put(pairs, 9)
        side.put(210 if ch == 0 else 180, 8)  # global_gain
        side.put(compress, 9)
        side.put(0, 1)  # no window switching
        for _ in range(3):
            side.put(1, 5)  # table_select
        side.put(15, 4)  # region0_count
        side.put(7, 3)  # region1_count
        side.put(0, 1)  # scalefac_scale
        side.put(0, 1)  # count1table_select
        main.bits += bits
    head = Bits()
    head.put(header, 32)
    return head.tobytes(4) + side.tobytes(SIDE) + main.tobytes(FRAME - 4 - SIDE)


def main():
    out = sys.argv[1]
    for scale in (0, 1):
        for mid_side in (0, 1):
            for slen in (2, 3, 4):
                name = f"l3_22k_intensity_s{scale}_ms{mid_side}_slen{slen}.mp3"
                with open(os.path.join(out, name), "wb") as f:
                    for i in range(23):
                        f.write(frame(scale, mid_side, slen, i < 3))


if __name__ == "__main__":
    main()
