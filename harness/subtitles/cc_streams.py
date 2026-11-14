#!/usr/bin/env python3
"""Deterministic closed-caption streams for the subtitle corpus.

Authors CEA-608 caption scripts (both fields: CC1 on field 1, CC3 on field 2)
and CEA-708 DTVCC packets, packs them frame by frame into ATSC A/53 `cc_data`
(cc_count constructs: the field 1 pair, the field 2 pair, DTVCC data and
invalid padding) and carries them in video:

* MPEG-2 video: A/53 `GA94` user data (or SCTE 20 user data) inserted before the
  first slice of every picture of a Homebrew `mpeg2video` encode, as a raw
  elementary stream (`.m2v`) and in MPEG-TS. The `.m2v` starts at the second,
  open GOP, so its leading B pictures have no forward reference.
* H.264 and HEVC: a `user_data_registered_itu_t_t35` SEI (`0xB5 0x0031 GA94`)
  inserted before the first slice of every access unit of a Homebrew libx264 or
  libx265 encode into MPEG-TS (the PES packets are rewritten with their PTS and
  DTS kept), then muxed with `ffmpeg -c copy` into Matroska and MP4.

Encodes and muxes use Homebrew `ffmpeg` (9.0.2) as an authoring tool; the
commands are in `encode` and `remux`. SCC files are written directly. The FATE
samples `sub/Closedcaption_rollup.m2v`, `sub/scte20.ts` and `sub/witch.scc` are
copied from `.research/fate-samples`.

With the reference worker built (`target/harness/ffmpeg-subtitles-worker`), the
manifest `harness/features/subtitle-cc/corpus.json` records its `sub-fold` of
every case, and `crates/subtitles/tests/cc/` receives the small test fixtures:
the two SCC scripts with the reference's events (`*.events`, one line per
event: packet, pts or `-`, end and the ASS lines, tab-separated) and a short
MPEG-2 stream with the reference's caption packets (`*.packets`: pts or `-`,
duration and the bytes in hex).

usage: cc_streams.py   (writes .research/subtitle-corpus/cc and fate/sub)
"""

import hashlib
import json
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORPUS = os.path.join(ROOT, ".research/subtitle-corpus")
OUT = os.path.join(CORPUS, "cc")
FATE = os.path.join(ROOT, ".research/fate-samples")
REFERENCE = os.path.join(ROOT, "target/harness/ffmpeg-subtitles-worker")
MANIFEST = os.path.join(ROOT, "harness/features/subtitle-cc/corpus.json")
FIXTURES = os.path.join(ROOT, "crates/subtitles/tests/cc")
FFMPEG = shutil.which("ffmpeg") or "ffmpeg"

# ------------------------------------------------------------------ CEA-608


def odd(byte):
    """`byte` (7 bits) with its odd parity bit."""
    byte &= 0x7F
    return byte | (0 if bin(byte).count("1") % 2 else 0x80)


def code(hi, lo):
    return (odd(hi), odd(lo))


def twice(hi, lo):
    """A control code as broadcast: sent twice."""
    return [code(hi, lo), code(hi, lo)]


def text(s):
    """Basic characters, two per pair; `^` marks no second character."""
    pairs = []
    raw = s.encode("latin-1")
    for i in range(0, len(raw), 2):
        a = raw[i]
        b = raw[i + 1] if i + 1 < len(raw) else 0
        pairs.append((odd(a), odd(b) if b else 0x80))
    return pairs


def pac(row, attribute=0, underline=False, channel=0x10):
    """Preamble address code: row 1-15, attribute 0-6 colour, 7 italics, 8-15 indent."""
    groups = {1: (0x11, 0), 2: (0x11, 1), 3: (0x12, 0), 4: (0x12, 1), 5: (0x15, 0),
              6: (0x15, 1), 7: (0x16, 0), 8: (0x16, 1), 9: (0x17, 0), 10: (0x17, 1),
              11: (0x10, 0), 12: (0x13, 0), 13: (0x13, 1), 14: (0x14, 0), 15: (0x14, 1)}
    hi, second = groups[row]
    lo = 0x40 | (second << 5) | (attribute << 1) | int(underline)
    return twice(hi | (channel & 0x08), lo)


def misc(command, field=1):
    """Miscellaneous control code (0x20-0x2F) of data channel 1 of `field`."""
    return twice(0x14 if field == 1 else 0x15, command)


RCL, BS, DER, RU2, RU3, RU4, RDC, TR, RTD, EDM, CR, ENM, EOC = (
    0x20, 0x21, 0x24, 0x25, 0x26, 0x27, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F)


def midrow(attribute, underline=False):
    return twice(0x11, 0x20 | (attribute << 1) | int(underline))


def special(n):
    return twice(0x11, 0x30 + n)


def extended(group, n, fallback):
    """An extended character after its basic fallback."""
    return text(fallback) + twice(0x12 + group, 0x20 + n)


def tab(n):
    return twice(0x17, 0x20 + n)


def background(color, semi=False):
    return twice(0x10, 0x20 | (color << 1) | int(semi))


# Field 1, CC1: pop-on captions with attributes and the character sets.
def popon_script(field=1):
    s = misc(RCL, field) + misc(ENM, field)
    s += pac(13, 8 + 1) + text("POP-ON ") + midrow(7) + text("ITALIC") + midrow(0) + text(" END")
    s += pac(14, 1, True) + text("GREEN") + midrow(2) + text(" BLUE") + midrow(5, True) + text(" YEL")
    s += pac(15, 7) + special(7) + text(" SONG ") + special(7)
    s += misc(EOC, field)
    s += [None] * 20
    s += misc(RCL, field) + pac(12, 4) + text("RED ") + extended(0, 4, "U") + text("BER ")
    s += extended(1, 0x14, "s") + text(" ") + extended(1, 0x1C, "+") + special(9) + text("x")
    s += pac(15, 0) + tab(3) + text("TAB") + background(1) + text(" BG") + background(7, True)
    s += text(" Caf") + extended(0, 0x13, "e") + text(" 'q'")
    s += misc(EOC, field)
    s += [None] * 20
    s += misc(EDM, field)
    s += [None] * 10
    s += misc(RCL, field) + pac(1) + text("TOP ROW") + pac(8, 3) + text("MIDDLE CYAN")
    s += misc(EOC, field) + [None] * 15 + misc(EDM, field)
    return s


# Field 2, CC3: roll-up of 2, 3 and 4 rows, paint-on, delete and text mode.
def rollup_script(field=2):
    s = misc(RU3, field) + pac(15, 0) + text("ROLL ONE")
    s += misc(CR, field) + text("ROLL TWO") + misc(CR, field) + text("ROLL THREE")
    s += misc(CR, field) + text("ROLL FOUR") + [None] * 6
    s += misc(RU2, field) + misc(CR, field) + text("TWO ROWS") + midrow(4) + text("RED")
    s += misc(RU4, field) + misc(CR, field) + text("FOUR") + misc(CR, field) + tab(2) + text("ROWS")
    s += [None] * 6
    s += misc(EDM, field)
    s += misc(RDC, field) + pac(5, 8 + 2) + text("PAINT ON") + [None] * 4
    s += pac(6, 0) + text("DELETE ME") + twice(0x14 if field == 1 else 0x15, 0x2D)[:0]
    s += pac(6, 8 + 1) + misc(DER, field) + text("KEPT")
    s += [None] * 8 + misc(RTD, field) + text("TEXT MODE") + misc(CR, field)
    s += [None] * 4 + misc(EDM, field)
    return s


# --------------------------------------------------------------- CEA-708


def dtvcc_packets(count):
    """DTVCC packets: service 1 blocks of text, then service 2."""
    packets = []
    for n in range(count):
        service = 1 if n % 3 else 2
        body = ("DTV%02d" % n).encode()
        block = bytes([(service << 5) | len(body)]) + body
        size_pairs = (1 + len(block) + 1) // 2
        header = ((n & 3) << 6) | size_pairs
        packet = bytes([header]) + block
        packet += b"\x00" * (size_pairs * 2 - len(packet))
        packets.append(packet)
    return packets


# ------------------------------------------------------------- cc_data


def frames(count, field1, field2, dtvcc=(), cc_count=10, field2_first=False):
    """`count` lists of cc_data constructs: one pair per field per frame, DTVCC
    packets two bytes per construct, padding up to cc_count."""
    f1 = list(field1)
    f2 = list(field2)
    pending = bytearray()
    queue = list(dtvcc)
    out = []
    for n in range(count):
        a = f1.pop(0) if f1 else None
        b = f2.pop(0) if f2 else None
        pair1 = [0xFC, *(a or (0x80, 0x80))]
        pair2 = [0xFD, *(b or (0x80, 0x80))]
        triplets = [pair2, pair1] if field2_first else [pair1, pair2]
        if not pending and queue and n % 6 == 3:
            pending = bytearray(queue.pop(0))
            triplets.append([0xFF, pending[0], pending[1]])
            del pending[:2]
        while pending and len(triplets) < cc_count:
            triplets.append([0xFE, pending[0], pending[1]])
            del pending[:2]
        while len(triplets) < cc_count:
            triplets.append([0xFA, 0x00, 0x00])
        out.append(bytes(b for t in triplets for b in t))
    return out


def a53_user_data(cc_data):
    """ATSC A/53 cc_data() inside `GA94` user data."""
    cc_count = len(cc_data) // 3
    return b"GA94\x03" + bytes([0x40 | cc_count, 0xFF]) + cc_data + b"\xff"


def reverse8(b):
    return int("{:08b}".format(b)[::-1], 2)


def scte20_user_data(entries):
    """SCTE 20 user data: entries of (field_number, byte1, byte2)."""
    bits = "{:05b}".format(len(entries))
    for field, b1, b2 in entries:
        bits += "00" + "{:02b}".format(field) + "{:05b}".format(11)
        bits += "{:08b}".format(reverse8(b1)) + "{:08b}".format(reverse8(b2)) + "1"
    bits += "0000"
    bits += "0" * (-len(bits) % 8)
    return b"\x03\x81" + bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))


# ---------------------------------------------------------- MPEG-2 video


def start_codes(es):
    i = es.find(b"\x00\x00\x01")
    while i >= 0:
        yield i
        i = es.find(b"\x00\x00\x01", i + 3)


def mpeg2_insert(es, payloads):
    """Inserts user data payloads[n] before the first slice of picture n."""
    out = bytearray()
    last = 0
    picture = -1
    pending = False
    for i in start_codes(es):
        kind = es[i + 3]
        if kind == 0x00:
            picture += 1
            pending = True
        elif 0x01 <= kind <= 0xAF and pending:
            pending = False
            out += es[last:i]
            for payload in payloads[picture] if picture < len(payloads) else ():
                out += b"\x00\x00\x01\xb2" + payload
            last = i
    out += es[last:]
    return bytes(out)


def mpeg2_from_second_gop(es):
    """The stream from its second sequence header on: an open GOP start."""
    headers = [i for i in start_codes(es) if es[i + 3] == 0xB3]
    return es[headers[1]:]


def mpeg2_pictures(es):
    return sum(1 for i in start_codes(es) if es[i + 3] == 0x00)


# ------------------------------------------------------------ H.264/HEVC


def escape(rbsp):
    out = bytearray()
    zeros = 0
    for b in rbsp:
        if zeros >= 2 and b <= 3:
            out.append(3)
            zeros = 0
        out.append(b)
        zeros = zeros + 1 if b == 0 else 0
    return bytes(out)


def nal_units(es):
    """(start, end, type) of each Annex B NAL unit, start at its start code."""
    starts = list(start_codes(es))
    for k, i in enumerate(starts):
        begin = i - 1 if i > 0 and es[i - 1] == 0 else i
        end = starts[k + 1] if k + 1 < len(starts) else len(es)
        if end < len(es) and es[end - 1] == 0:
            end -= 1
        yield begin, end, es[i + 3]


def annexb_insert(es, seis, hevc):
    """Inserts seis[n] before the first slice of access unit n (AUD-delimited)."""
    out = bytearray()
    au = -1
    pending = False
    for begin, end, header in nal_units(es):
        kind = (header >> 1) & 0x3F if hevc else header & 0x1F
        if kind == (35 if hevc else 9):
            au += 1
            pending = True
        vcl = kind < 32 if hevc else 1 <= kind <= 5
        if vcl and pending:
            pending = False
            if au < len(seis):
                out += seis[au]
        out += es[begin:end]
    return bytes(out)


def reserved_before_slice(au):
    """`au` (HEVC) with a reserved VCL NAL unit (type 10) before its first slice."""
    for begin, _, header in nal_units(au):
        if (header >> 1) & 0x3F < 32:
            return au[:begin] + b"\0\0\x01\x14\x01\x80" + au[begin:]
    return au


# ------------------------------------------------- header rewrites (H.264/HEVC)
#
# The crafted picture output cases change header fields an encoder option
# cannot reach. Each rewrite reads the RBSP bit by bit and writes it back with
# the fields replaced; slices whose header changes length are CAVLC encodes.


def unescape(data):
    out, zeros = bytearray(), 0
    for byte in data:
        if zeros >= 2 and byte == 3:
            zeros = 0
            continue
        out.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return bytes(out)


class BitReader:
    def __init__(self, rbsp):
        self.data = rbsp
        self.pos = 0

    def u(self, n):
        v = 0
        for _ in range(n):
            byte = self.data[self.pos >> 3] if self.pos >> 3 < len(self.data) else 0
            v = (v << 1) | ((byte >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return v

    def ue(self):
        zeros = 0
        while self.u(1) == 0:
            zeros += 1
        return (1 << zeros) - 1 + self.u(zeros)

    def se(self):
        k = self.ue()
        return (k + 1) // 2 if k & 1 else -(k // 2)

    def left(self):
        return len(self.data) * 8 - self.pos


class BitWriter:
    def __init__(self):
        self.bits = []

    def u(self, v, n):
        self.bits += [(v >> (n - 1 - i)) & 1 for i in range(n)]

    def ue(self, v):
        code = v + 1
        width = code.bit_length()
        self.u(0, width - 1)
        self.u(code, width)

    def se(self, v):
        self.ue(2 * v - 1 if v > 0 else -2 * v)

    def rbsp(self):
        bits = self.bits + [0] * (-len(self.bits) % 8)
        return bytes(int("".join(map(str, bits[i:i + 8])), 2) for i in range(0, len(bits), 8))


class Copy:
    """Reads fields, writing them unchanged unless the caller writes others."""

    def __init__(self, nal, header_len):
        self.head = nal[:header_len]
        self.src = BitReader(unescape(nal[header_len:]))
        self.dst = BitWriter()

    def u(self, n):
        v = self.src.u(n)
        self.dst.u(v, n)
        return v

    def ue(self):
        v = self.src.ue()
        self.dst.ue(v)
        return v

    def se(self):
        v = self.src.se()
        self.dst.se(v)
        return v

    def finish(self):
        while self.src.left() > 0:
            self.dst.u(self.src.u(1), 1)
        return self.head + escape(self.dst.rbsp().rstrip(b"\x00"))


def map_nals(au, change):
    """`au` with each NAL unit (from its header) replaced by change(nal)."""
    out = bytearray()
    last = 0
    for begin, end, _ in nal_units(au):
        unit = au[begin:end]
        nal = unit[unit.index(b"\x00\x00\x01") + 3:]
        out += au[last:begin] + b"\x00\x00\x00\x01" + change(nal)
        last = end
    return bytes(out + au[last:])


HIGH_PROFILES = (100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135)


def avc_sps_fields(b):
    """Walks an H.264 SPS with `b` (a Copy) up to the VUI; returns the fields
    slices need."""
    f = {}
    profile = b.u(8)
    b.u(16)
    b.ue()
    f["chroma"] = 1
    if profile in HIGH_PROFILES:
        f["chroma"] = b.ue()
        if f["chroma"] == 3:
            b.u(1)
        b.ue()
        b.ue()
        b.u(1)
        if b.u(1):
            for i in range(8 if f["chroma"] != 3 else 12):
                if b.u(1):
                    last, nxt = 8, 8
                    for _ in range(16 if i < 6 else 64):
                        if nxt:
                            nxt = (last + b.se() + 256) % 256
                        last = nxt or last
    f["fn_bits"] = b.ue() + 4
    f["poc_type"] = b.ue()
    if f["poc_type"] == 0:
        f["lsb_bits"] = b.ue() + 4
    elif f["poc_type"] == 1:
        f["always_zero"] = b.u(1)
        b.se()
        b.se()
        for _ in range(b.ue()):
            b.se()
    b.ue()
    b.u(1)
    b.ue()
    b.ue()
    f["frame_mbs_only"] = b.u(1)
    if not f["frame_mbs_only"]:
        b.u(1)
    b.u(1)
    if b.u(1):
        for _ in range(4):
            b.ue()
    return f


def avc_set_restriction(nal, restriction, timing=None):
    """An H.264 SPS with its VUI bitstream restriction replaced by
    `restriction` ((max_num_reorder_frames, max_dec_frame_buffering) or None
    to drop it) and, with `timing`, its timing (num_units_in_tick, time_scale)."""
    b = Copy(nal, 1)
    avc_sps_fields(b)
    assert b.u(1), "an SPS with VUI"
    if b.u(1) and b.u(8) == 255:
        b.u(32)
    if b.u(1):
        b.u(1)
    if b.u(1):
        b.u(4)
        if b.u(1):
            b.u(24)
    if b.u(1):
        b.ue()
        b.ue()
    if b.src.u(1):
        old = (b.src.u(32), b.src.u(32), b.src.u(1))
        b.dst.u(1, 1)
        num, scale = timing or old[:2]
        b.dst.u(num, 32)
        b.dst.u(scale, 32)
        b.dst.u(old[2], 1)
    else:
        b.dst.u(0, 1)
    for _ in range(2):  # no HRD in libx264's VUI
        assert not b.u(1)
    b.u(1)  # pic_struct_present_flag
    old = None
    if b.src.u(1):
        old = [b.src.u(1)] + [b.src.ue() for _ in range(6)]
    if restriction is None:
        b.dst.u(0, 1)
    else:
        b.dst.u(1, 1)
        fields = old or [1, 2, 1, 16, 16, 0, 0]
        b.dst.u(fields[0], 1)
        for v in fields[1:5] + list(restriction):
            b.dst.ue(v)
    b.dst.u(1, 1)
    return b.head + escape(b.dst.rbsp())


class AvcSlices:
    """Rewrites H.264 slice headers: frame_num, the order count LSB width and
    value, and dec_ref_pic_marking."""

    def __init__(self):
        self.sps = {}
        self.pps = {}

    def see(self, nal):
        kind = nal[0] & 0x1F
        if kind == 7:
            r = BitReader(unescape(nal[1:]))
            r.u(24)
            self.sps[r.ue()] = avc_sps_fields(Copy(nal, 1))
        elif kind == 8:
            r = BitReader(unescape(nal[1:]))
            pid, sid, _, bottom = r.ue(), r.ue(), r.u(1), r.u(1)
            assert r.ue() == 0
            l0, l1 = r.ue() + 1, r.ue() + 1
            wp, wbi = r.u(1), r.u(2)
            r.se(), r.se(), r.se(), r.u(1), r.u(1)
            self.pps[pid] = dict(sps=sid, bottom=bottom, l0=l0, l1=l1, wp=wp, wbi=wbi, redundant=r.u(1))

    def rewrite(self, nal, frame_num=None, marking=None):
        """frame_num(old) -> new; marking(ops) -> ops for a non-IDR reference
        slice (ops: (operation, args) pairs) or the IDR's
        no_output_of_prior_pics_flag as `marking(None)`."""
        kind = nal[0] & 0x1F
        ref = nal[0] >> 5
        b = Copy(nal, 1)
        b.ue()
        st = b.ue() % 5
        p = self.pps[b.ue()]
        s = self.sps[p["sps"]]
        fn = b.src.u(s["fn_bits"])
        b.dst.u(frame_num(fn) if frame_num else fn, s["fn_bits"])
        field = 0
        if not s["frame_mbs_only"]:
            field = b.u(1)
            if field:
                b.u(1)
        if kind == 5:
            b.ue()
        if s["poc_type"] == 0:
            b.u(s["lsb_bits"])
            if p["bottom"] and not field:
                b.se()
        if s["poc_type"] == 1 and not s["always_zero"]:
            b.se()
            if p["bottom"] and not field:
                b.se()
        if p["redundant"]:
            b.ue()
        if st == 1:
            b.u(1)
        l0, l1 = p["l0"], p["l1"]
        if st in (0, 1, 3) and b.u(1):
            l0 = b.ue() + 1
            if st == 1:
                l1 = b.ue() + 1
        for _ in range((2 if st == 1 else 1) if st in (0, 1, 3) else 0):
            if b.u(1):
                while b.ue() != 3:
                    b.ue()
        if (p["wp"] and st in (0, 3)) or (p["wbi"] == 1 and st == 1):
            b.ue()
            chroma = s["chroma"] != 0
            if chroma:
                b.ue()
            for count in ((l0, l1) if st == 1 else (l0,)):
                for _ in range(count):
                    if b.u(1):
                        b.se()
                        b.se()
                    if chroma and b.u(1):
                        for _ in range(4):
                            b.se()
        if ref:
            if kind == 5:
                old = b.src.u(1)
                b.dst.u(int(marking(None)) if marking else old, 1)
                b.u(1)
            else:
                ops = []
                if b.src.u(1):
                    while True:
                        op = b.src.ue()
                        if op == 0:
                            break
                        args = [b.src.ue() for _ in range({1: 1, 2: 1, 3: 2, 4: 1, 6: 1}.get(op, 0))]
                        ops.append((op, args))
                new = marking(ops) if marking else ops
                b.dst.u(1 if new else 0, 1)
                if new:
                    for op, args in new:
                        b.dst.ue(op)
                        for a in args:
                            b.dst.ue(a)
                    b.dst.ue(0)
        return b.finish(), dict(type=kind, ref=ref, slice_type=st, frame_num=fn)


def avc_edit(ts, transform_slice=None, transform_sps=None, transform_au=None):
    """`ts` (H.264 in MPEG-TS) with each SPS passed through transform_sps(nal),
    each slice through transform_slice(n, slices, nal) and each access unit
    through transform_au(n, au) (None drops it)."""
    slices = AvcSlices()

    def each(n, au):
        if transform_au:
            au = transform_au(n, au)
            if au is None:
                return None

        def change(nal):
            kind = nal[0] & 0x1F
            if kind == 7 and transform_sps:
                nal = transform_sps(nal)
            if kind in (7, 8):
                slices.see(nal)
            if kind in (1, 5) and transform_slice:
                return transform_slice(n, slices, nal)
            return nal
        return map_nals(au, change)
    return ts_rewrite_video(ts, each)


def avc_mmco5(at):
    """A slice transform: the reference pictures `at(n)` names get
    memory_management_control_operation 5, and the frame_num of the pictures
    after each, until the next IDR picture, is renumbered so the next
    reference picture has frame_num 1."""
    state = {"shift": None}

    def transform(n, slices, nal):
        if nal[0] & 0x1F == 5:
            state["shift"] = None
            return nal
        if at(n):
            shift = state["shift"] or 0
            bits = slices.sps[0]["fn_bits"]
            new, fields = slices.rewrite(nal, frame_num=lambda fn: (fn - shift) % (1 << bits),
                                         marking=lambda ops: ops + [(5, [])])
            state["shift"] = fields["frame_num"]
            return new
        if state["shift"] is not None:
            bits = slices.sps[0]["fn_bits"]
            return slices.rewrite(nal, frame_num=lambda fn: (fn - state["shift"]) % (1 << bits))[0]
        return nal
    return transform


def set_recovery_frame_cnt(au, count):
    """An H.264 access unit with its recovery point SEI's recovery_frame_cnt
    set to `count`."""
    w = BitWriter()
    w.ue(count)
    w.u(1, 1)
    w.u(0, 1)
    w.u(0, 2)
    payload = w.rbsp()

    def change(nal):
        if nal[0] & 0x1F == 6 and nal[1] == 6:
            return b"\x06\x06" + bytes([len(payload)]) + payload + b"\x80"
        return nal
    return map_nals(au, change)


def hevc_ptl_copy(b, sub_layers):
    b.u(96)
    present = [(b.u(1), b.u(1)) for _ in range(sub_layers)]
    if sub_layers > 0:
        b.u(2 * (8 - sub_layers))
    for profile, level in present:
        if profile:
            b.u(88)
        if level:
            b.u(8)


def hevc_set_ordering(au, dpb, reorder, latency):
    """An HEVC access unit whose SPS states sps_max_dec_pic_buffering_minus1
    `dpb`, sps_max_num_reorder_pics `reorder` and
    sps_max_latency_increase_plus1 `latency` for every sub-layer."""
    return map_nals(au, lambda nal: hevc_set_ordering_nal(nal, dpb, reorder, latency)
                    if (nal[0] >> 1) & 0x3F == 33 else nal)


def hevc_sps_id(nal, sps_id):
    """An HEVC SPS NAL unit with `sps_seq_parameter_set_id` `sps_id`."""
    b = Copy(nal, 2)
    b.u(4)
    sub = b.u(3)
    b.u(1)
    hevc_ptl_copy(b, sub)
    b.src.ue()
    b.dst.ue(sps_id)
    return b.finish()


def hevc_sps_latency(nal, latency):
    """An HEVC SPS NAL unit with `sps_max_latency_increase_plus1` `latency`
    for every sub-layer."""
    return hevc_set_ordering_nal(nal, None, None, latency)


def hevc_set_ordering_nal(nal, dpb, reorder, latency):
    """An HEVC SPS NAL unit with the sub-layer ordering values that are not
    None replaced for every sub-layer."""
    b = Copy(nal, 2)
    b.u(4)
    sub = b.u(3)
    b.u(1)
    hevc_ptl_copy(b, sub)
    b.ue()
    if b.ue() == 3:
        b.u(1)
    b.ue()
    b.ue()
    if b.u(1):
        for _ in range(4):
            b.ue()
    b.ue()
    b.ue()
    b.ue()
    all_layers = b.u(1)
    for _ in range(0 if all_layers else sub, sub + 1):
        for value in (dpb, reorder, latency):
            old = b.src.ue()
            b.dst.ue(old if value is None else value)
    return b.finish()


def set_vui_timing(au, old, new, hevc):
    """`au` with its SPS's VUI timing (num_units_in_tick, time_scale) `old`
    replaced by `new` (fields of the same width)."""
    def change(nal):
        kind = (nal[0] >> 1) & 0x3F if hevc else nal[0] & 0x1F
        if kind != (33 if hevc else 7):
            return nal
        head = 2 if hevc else 1
        bits = "".join("{:08b}".format(b) for b in unescape(nal[head:]))
        at = bits.find("{:032b}{:032b}".format(*old))
        bits = bits[:at] + "{:032b}{:032b}".format(*new) + bits[at + 64:]
        rbsp = bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits) - len(bits) % 8, 8))
        return nal[:head] + escape(rbsp)
    return map_nals(au, change)


# ------------------------------------------------------ field-coded H.264
#
# libx264 codes no field pictures, so these are written directly: 16x32
# frames as a top and a bottom field each (I fields of one I_PCM macroblock,
# P and B fields of one skipped macroblock), I P B B order, order count type 0.


def field_sps(restriction=(2, 4)):
    """An SPS of 16x32 field pairs, VUI timing 1001/60000 and the bitstream
    restriction (reorder, buffering)."""
    b = BitWriter()
    for v, n in ((77, 8), (0, 8), (30, 8)):
        b.u(v, n)
    for v in (0, 0, 0, 4, 4):
        b.ue(v)  # id, frame_num 4 bits, POC type 0, 8-bit LSBs, 4 reference frames
    b.u(0, 1)
    b.ue(0)
    b.ue(0)  # 16x32
    for v in (0, 0, 1, 0, 1):
        b.u(v, 1)  # fields, no MBAFF, direct 8x8, no crop, VUI
    b.u(0, 4)
    b.u(1, 1)
    b.u(1001, 32)
    b.u(60000, 32)
    b.u(1, 1)
    b.u(0, 3)
    b.u(1, 1)
    b.u(1, 1)
    for v in (0, 0, 16, 16) + tuple(restriction):
        b.ue(v)
    b.u(1, 1)
    return b"\x67" + escape(b.rbsp())


def field_pps():
    b = BitWriter()
    for v in (0, 0):
        b.ue(v)
    b.u(0, 2)
    for v in (0, 0, 0):
        b.ue(v)
    b.u(0, 3)
    for _ in range(3):
        b.se(0)
    b.u(0, 3)
    b.u(1, 1)
    return b"\x68" + escape(b.rbsp())


def field_slice(kind, idr, ref, frame_num, bottom, lsb):
    b = BitWriter()
    b.ue(0)
    b.ue({"P": 5, "B": 6, "I": 7}[kind])
    b.ue(0)
    b.u(frame_num, 4)
    b.u(1, 1)
    b.u(int(bottom), 1)
    if idr:
        b.ue(0)
    b.u(lsb, 8)
    if kind == "B":
        b.u(1, 1)
    if kind != "I":
        b.u(0, 2 if kind == "P" else 3)
    if ref:
        b.u(0, 2 if idr else 1)
    b.se(0)
    if kind == "I":
        b.ue(25)  # I_PCM
        b.u(0, -len(b.bits) % 8)
        for _ in range(384):
            b.u(0x80, 8)
    else:
        b.ue(1)  # mb_skip_run
    b.u(1, 1)
    return bytes([0x65 if idr else 0x41 if ref else 0x01]) + escape(b.rbsp())


def frame_sps(restriction=None, size=(2, 2), level=30, gaps=False):
    """An SPS of 32x32 frames (or `size` macroblocks): frame_num 4 bits, POC
    type 0 with 8-bit LSBs, 2 reference frames, level 3.0 (or `level`),
    gaps_in_frame_num_value_allowed_flag `gaps`; with `restriction` (reorder,
    buffering) a VUI bitstream restriction, else no VUI."""
    b = BitWriter()
    for v, n in ((77, 8), (0, 8), (level, 8)):
        b.u(v, n)
    for v in (0, 0, 0, 4, 2):
        b.ue(v)
    b.u(1 if gaps else 0, 1)
    b.ue(size[0] - 1)
    b.ue(size[1] - 1)
    for v in (1, 1, 0):
        b.u(v, 1)  # frames, direct 8x8, no crop
    b.u(1 if restriction else 0, 1)
    if restriction:
        b.u(0, 4)
        b.u(0, 1)  # no timing
        b.u(0, 3)
        b.u(1, 1)
        b.u(1, 1)
        for v in (0, 0, 16, 16) + tuple(restriction):
            b.ue(v)
    b.u(1, 1)
    return b"\x67" + escape(b.rbsp())


def frame_slice(kind, idr, ref, frame_num, lsb, mbs=4):
    """A slice of a 32x32 frame (or of `mbs` macroblocks; field_pps): I_PCM
    or skipped macroblocks."""
    b = BitWriter()
    b.ue(0)
    b.ue({"P": 5, "B": 6, "I": 7}[kind])
    b.ue(0)
    b.u(frame_num % 16, 4)
    if idr:
        b.ue(0)
    b.u(lsb % 256, 8)
    if kind == "B":
        b.u(1, 1)
    if kind != "I":
        b.u(0, 2 if kind == "P" else 3)
    if ref:
        b.u(0, 2 if idr else 1)
    b.se(0)
    if kind == "I":
        for _ in range(mbs):
            b.ue(25)  # I_PCM
            b.u(0, -len(b.bits) % 8)
            for _ in range(384):
                b.u(0x80, 8)
    else:
        b.ue(mbs)  # mb_skip_run
    b.u(1, 1)
    return bytes([0x65 if idr else 0x41 if ref else 0x01]) + escape(b.rbsp())


def recovery_point_sei(count):
    """An H.264 recovery point SEI NAL unit: recovery_frame_cnt `count`,
    exact match, no broken link."""
    b = BitWriter()
    b.ue(count)
    b.u(1, 1)
    b.u(0, 3)
    payload = b.rbsp()
    return b"\x00\x00\x00\x01\x06" + escape(bytes([6, len(payload)]) + payload + b"\x80")


def start_pictures(kinds, steps=None, refs=None, frame_num=5, count=300):
    """(kind, idr, ref, frame_num, order count) of a stream without an IDR
    picture: `kinds` gives the first pictures' kinds (then P frames); order
    counts from 40 in steps of 2, or steps[n] where given; refs[n] False makes
    picture n a non-reference picture; frame_num from `frame_num`."""
    out, fn, poc = [], frame_num, 40
    for n in range(count):
        if n:
            poc += (steps or {}).get(n, 2)
        kind = kinds[n] if n < len(kinds) else "P"
        ref = (refs or {}).get(n, kind != "B")
        out.append((kind, False, ref, fn, poc))
        fn += 1 if ref else 0
    return out


def matroska(units, scale=1_000_000):
    """A Matroska file of one H.264 track (avcC from the first unit's SPS and
    PPS) whose SimpleBlocks carry no duration, without DefaultDuration:
    units are (time in nanoseconds, Annex B access unit) in decode order,
    the first a keyframe."""
    def vint(n):
        length = next(k for k in range(1, 9) if n < (1 << (7 * k)) - 1)
        return ((1 << (7 * length)) | n).to_bytes(length, "big")

    def element(eid, body):
        return eid + vint(len(body)) + body

    def uint(eid, v):
        return element(eid, v.to_bytes(max(1, (v.bit_length() + 7) // 8), "big"))

    def nals(au):
        return [part.rstrip(b"\x00") for part in au.split(b"\x00\x00\x01")[1:]]

    first = nals(units[0][1])
    sps = next(n for n in first if n[0] & 0x1F == 7)
    pps = next(n for n in first if n[0] & 0x1F == 8)
    avcc = (bytes([1, sps[1], sps[2], sps[3], 0xFF, 0xE1]) + len(sps).to_bytes(2, "big") + sps
            + b"\x01" + len(pps).to_bytes(2, "big") + pps)
    header = element(b"\x1a\x45\xdf\xa3", uint(b"\x42\x86", 1) + uint(b"\x42\xf7", 1) + uint(b"\x42\xf2", 4)
                     + uint(b"\x42\xf3", 8) + element(b"\x42\x82", b"matroska") + uint(b"\x42\x87", 4)
                     + uint(b"\x42\x85", 2))
    info = element(b"\x15\x49\xa9\x66", uint(b"\x2a\xd7\xb1", scale) + element(b"\x4d\x80", b"t")
                   + element(b"\x57\x41", b"t"))
    track = element(b"\xae", uint(b"\xd7", 1) + uint(b"\x73\xc5", 1) + uint(b"\x83", 1)
                    + element(b"\x86", b"V_MPEG4/ISO/AVC") + element(b"\x63\xa2", avcc)
                    + element(b"\xe0", uint(b"\xb0", 32) + uint(b"\xba", 32)))
    clusters, blocks, base = b"", [], None
    for n, (ns, au) in enumerate(units):
        t = ns // scale
        if base is None or not -32000 <= t - base <= 32000:
            if blocks:
                clusters += element(b"\x1f\x43\xb6\x75", uint(b"\xe7", base) + b"".join(blocks))
            blocks, base = [], t
        payload = b"".join(len(x).to_bytes(4, "big") + x for x in nals(au) if x[0] & 0x1F not in (7, 8, 9))
        block = vint(1) + (t - base).to_bytes(2, "big", signed=True) + bytes([0x80 if n == 0 else 0]) + payload
        blocks.append(element(b"\xa3", block))
    if blocks:
        clusters += element(b"\x1f\x43\xb6\x75", uint(b"\xe7", base) + b"".join(blocks))
    segment = (b"\x18\x53\x80\x67" + b"\x01\xff\xff\xff\xff\xff\xff\xff" + info
               + element(b"\x16\x54\xae\x6b", track) + clusters)
    return header + segment


def frame_pictures(count, bs=(), start=None, ifr=None, jumps=(), gaps=()):
    """(kind, idr, ref, frame_num, order count) of P frames with order counts
    2 apart from an IDR picture; `bs` maps a decode index to (P frames above
    it, ...) for a non-reference B picture; `start` (index of a non-IDR I
    frame) begins without an IDR picture; the P frames at the decode indices
    in `jumps` are 4 above the one before, and those in `gaps` skip one
    frame_num."""
    out, fn, pk = [], 0, 0
    pocs = []
    base, offset = (0, 0) if start is None else (40, 2)
    for n in range(count):
        if n in bs:
            above = bs[n]
            out.append(("B", False, False, fn + 1, pocs[-above] - 1 if above else pocs[-1] + 1))
            continue
        if n == 0 and start is None:
            out.append(("I", True, True, 0, 0)); pocs.append(0); continue
        fn += 2 if n in gaps else 1
        pk += 2 if n in jumps else 1
        kind = "I" if start is not None and n == start else "P"
        poc = base + 2 * pk
        out.append((kind, False, True, fn + offset, poc)); pocs.append(poc)
    return out


def field_pictures(count):
    """(slice NAL, display index) of each field, in decode order."""
    frames = [("I", 0, 0)]
    fn = d = 0
    while len(frames) < count:
        fn += 1
        frames += [("P", d + 3, fn), ("B", d + 1, fn + 1), ("B", d + 2, fn + 1)]
        d += 3
    fields = []
    for k, (kind, d, fn) in enumerate(frames[:count]):
        for bottom in (False, True):
            nal = field_slice(kind, k == 0 and not bottom, kind != "B", fn % 16, bottom, (4 * d + bottom) % 256)
            fields.append((nal, d))
    return fields


# --------------------------------------------------------------- encodes


def run(args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def encode(codec, frames_count, path, open_gop=False, params="", size="64x48"):
    """The base video: a small test pattern at 29.97 Hz with B pictures; MPEG-2
    as an elementary stream, H.264 and HEVC in MPEG-TS. `open_gop` makes the
    H.264 and HEVC keyframes after the first non-IDR I pictures (with recovery
    point SEI) and CRA pictures. `params` replaces the B picture options of
    the libx264 and libx265 parameters."""
    source = ["-f", "lavfi", "-i", "testsrc=size=%s:rate=30000/1001" % size]
    common = [FFMPEG, "-v", "error", "-y", "-fflags", "+bitexact"] + source + [
        "-frames:v", str(frames_count), "-flags", "+bitexact", "-threads", "1"]
    if codec == "mpeg2":
        run(common + ["-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-b:v", "200k", "-f", "mpeg2video", path])
    elif codec == "h264":
        run(common + ["-c:v", "libx264", "-preset", "veryfast",
                      "-x264-params", "aud=1:keyint=12:min-keyint=12:scenecut=0:threads=1:"
                      + (params or "bframes=2:b-adapt=0:b-pyramid=none")
                      + (":open-gop=1" if open_gop else ""),
                      "-f", "mpegts", path])
    else:
        run(common + ["-c:v", "libx265", "-preset", "veryfast",
                      "-x265-params", "aud=1:" + (params or "bframes=2:b-adapt=0:b-pyramid=0")
                      + ":keyint=12:min-keyint=12:scenecut=0:pools=1:frame-threads=1:log-level=error"
                      + (":open-gop=1" if open_gop else ""),
                      "-f", "mpegts", path])


def remux(source, path, raw_format=None):
    """`source` in another container, packets copied. A raw elementary stream
    (`raw_format`) gets generated timestamps at 29.97 Hz."""
    container = {".ts": "mpegts", ".mkv": "matroska", ".mp4": "mp4"}[os.path.splitext(path)[1]]
    raw = ["-fflags", "+bitexact+genpts", "-r", "30000/1001", "-f", raw_format] if raw_format else ["-fflags", "+bitexact"]
    run([FFMPEG, "-v", "error", "-y"] + raw + ["-i", source, "-c", "copy", "-flags", "+bitexact",
         "-f", container, path])


def ts_rewrite_video(ts, transform, pts=None):
    """`ts` with each video PES payload n replaced by transform(n, payload), or
    dropped when it returns None; the PES headers (PTS, DTS) and the other
    packets stay, but for the PTS of a PES without DTS, which pts(n) gives
    when passed (None leaves the PES without a time). The rewritten PES has
    PES_packet_length 0 and is cut into packets, the last stuffed."""
    packets = [ts[i:i + 188] for i in range(0, len(ts), 188)]
    video = None
    for p in packets:
        pid = ((p[1] & 0x1F) << 8) | p[2]
        if pid == 0x1000 and p[1] & 0x40:  # the PMT of ffmpeg's muxer
            body = p[5 + p[4]:]
            info = ((body[10] & 0x0F) << 8) | body[11]
            video = ((body[13 + info] & 0x1F) << 8) | body[14 + info]
    out = bytearray()
    pes = []

    def flush():
        if not pes:
            return
        first_af, data = pes[0]
        header_length = 9 + data[8]
        header = bytearray(data[:header_length])
        header[4:6] = b"\x00\x00"
        payload = transform(flush.count, bytes(data[header_length:]))
        if payload is not None and pts is not None:
            assert header[7] >> 6 == 2, "a PES with a DTS"
            t = pts(flush.count)
            if t is None:
                header = header[:7] + bytes([header[7] & 0x3F, header[8] - 5]) + header[14:]
            else:
                t %= 1 << 33
                header[9:14] = bytes([0x21 | (t >> 29) & 0x0E, (t >> 22) & 0xFF, 0x01 | (t >> 14) & 0xFE,
                                      (t >> 7) & 0xFF, 0x01 | (t << 1) & 0xFE])
        flush.count += 1
        pes.clear()
        if payload is None:
            return
        stream = bytes(header) + payload
        cc = flush.cc
        first = True
        while stream:
            af = first_af if first else b""
            room = 184 - len(af)
            chunk = stream[:room]
            stream = stream[room:]
            if len(chunk) < room:
                # The adaptation field grows by the bytes the payload leaves.
                stuffing = room - len(chunk)
                if af:
                    af = bytes([af[0] + stuffing]) + af[1:] + b"\xff" * stuffing
                else:
                    af = bytes([stuffing - 1]) + (b"\x00" + b"\xff" * (stuffing - 2) if stuffing > 1 else b"")
            flags = 0x30 if af else 0x10
            pusi = 0x40 if first else 0
            out.extend(bytes([0x47, pusi | (video >> 8), video & 0xFF, flags | cc]))
            out.extend(af)
            out.extend(chunk)
            cc = (cc + 1) & 0x0F
            first = False
        flush.cc = cc
        pes.clear()

    flush.count = 0
    flush.cc = 0
    for p in packets:
        pid = ((p[1] & 0x1F) << 8) | p[2]
        if pid != video:
            out.extend(p)
            continue
        control = (p[3] >> 4) & 3
        offset = 4
        af = b""
        if control & 2:
            af = p[4:5 + p[4]]
            offset = 5 + p[4]
        if p[1] & 0x40:
            flush()
            pes.append((af, bytearray(p[offset:])))
        elif pes:
            pes[0][1].extend(p[offset:])
    flush()
    return bytes(out)


# ----------------------------------------------------------------- cases


def write(name, data):
    path = os.path.join(OUT, name)
    with open(path, "wb") as f:
        f.write(data)
    return path


def scc(name, lines):
    """An SCC file: (frame number, pairs) lines at 29.97 Hz non-drop time codes."""
    out = ["Scenarist_SCC V1.0", ""]
    for frame, pairs in lines:
        seconds, f = divmod(frame, 30)
        tc = "%02d:%02d:%02d:%02d" % (seconds // 3600, seconds // 60 % 60, seconds % 60, f)
        out.append(tc + "\t" + " ".join("%02x%02x" % p for p in pairs))
        out.append("")
    return write(name, ("\n".join(out) + "\n").encode())


def caption_frames(count, field2_first=False, dtvcc=True):
    """CC1 pop-on on field 1, CC3 roll-up on field 2 and DTVCC packets."""
    return frames(count, popon_script(1), rollup_script(2), dtvcc_packets(12) if dtvcc else (),
                  field2_first=field2_first)


def mpeg2_display_order(es):
    """The display index of each picture, in decode order: its GOP's first
    display index plus its temporal reference."""
    order = []
    base = 0
    span = 0
    for i in start_codes(es):
        kind = es[i + 3]
        if kind == 0xB8:
            base += span
            span = 0
        elif kind == 0x00:
            reference = (es[i + 4] << 2) | (es[i + 5] >> 6)
            span = max(span, reference + 1)
            order.append(base + reference)
    return order


def mpeg2_set_top_field_first(es):
    """`es` with top_field_first set in every picture coding extension."""
    out = bytearray(es)
    for i in start_codes(es):
        if es[i + 3] == 0xB5 and es[i + 4] >> 4 == 8:
            out[i + 7] |= 0x80
    return bytes(out)


def mpeg2_set_display(es, progressive_sequence, flags):
    """`es` with progressive_sequence set or cleared and picture n (decode
    order) given flags(n) = (top_field_first, repeat_first_field,
    progressive_frame)."""
    out = bytearray(es)
    n = -1
    for i in start_codes(es):
        if es[i + 3] == 0x00:
            n += 1
        elif es[i + 3] == 0xB5 and es[i + 4] >> 4 == 1:
            out[i + 5] = (out[i + 5] & ~0x08) | (0x08 if progressive_sequence else 0)
        elif es[i + 3] == 0xB5 and es[i + 4] >> 4 == 8:
            top, repeat, progressive = flags(n)
            out[i + 7] = (out[i + 7] & ~0x82) | (0x80 if top else 0) | (0x02 if repeat else 0)
            out[i + 8] = (out[i + 8] & 0x7F) | (0x80 if progressive else 0)
    return bytes(out)


def mpeg2_field_pictures(es):
    """`es` with every frame picture coded as a top and a bottom field picture
    (headers and slices repeated; the slices then decode as damaged)."""
    bounds = [i for i in start_codes(es) if es[i + 3] in (0x00, 0xB3, 0xB8)] + [len(es)]
    out = bytearray(es[:bounds[0]])
    for a, b in zip(bounds, bounds[1:]):
        unit = es[a:b]
        if es[a + 3] != 0x00:
            out += unit
            continue
        for structure in (1, 2):
            field = bytearray(unit)
            for j in start_codes(unit):
                if unit[j + 3] == 0xB5 and unit[j + 4] >> 4 == 8:
                    field[j + 6] = (field[j + 6] & 0xFC) | structure
            out += field
    return bytes(out)


def strip_recovery_points(au):
    """An H.264 access unit without its recovery point SEI NAL units."""
    out = bytearray()
    for begin, end, header in nal_units(au):
        nal = au[begin:end]
        body = nal[nal.index(b"\x00\x00\x01") + 3:]
        if header & 0x1F == 6 and body[1] == 6:
            continue
        out += nal
    return bytes(out)


def dvd_user_data(cc_data, odd_field_first=True, header_count=1):
    """DVD-Video line 21 user data of one caption block: the field 1 and field 2
    pairs of `cc_data`, in the order `odd_field_first` names."""
    words = [(0xFF, cc_data[1], cc_data[2]), (0xFE, cc_data[4], cc_data[5])]
    if not odd_field_first:
        words.reverse()
    attribute = (0x80 if odd_field_first else 0) | (header_count << 1)
    return b"CC\x01\xf8" + bytes([attribute]) + bytes(b for w in words for b in w)


def dish_user_data(cc_data, n):
    """Dish Network caption user data (FFmpeg's reading of it) with the field 1
    pair of `cc_data`: type 2 (sent twice when a control code), and every fifth
    picture behind a type 5 header."""
    a, b = cc_data[1], cc_data[2]
    body = bytes([0x02, 0x00, a, b, 0x04 if a & 0x7F < 0x20 else 0x00, 0x00])
    if n % 5 == 4:
        body = bytes([0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]) + body
    return bytes([0x05, 0x02, 0xCD, 0xE6, 0x00, 0x01, 0x00]) + body


def strip_vui_timing(au):
    """An H.264 access unit whose SPS (from libx264 at 30000/1001 Hz) has its VUI
    timing information removed: timing_info_present_flag cleared and the 65
    bits after it dropped."""
    def unescape(data):
        out, zeros = bytearray(), 0
        for byte in data:
            if zeros >= 2 and byte == 3:
                zeros = 0
                continue
            out.append(byte)
            zeros = zeros + 1 if byte == 0 else 0
        return bytes(out)

    out = bytearray()
    for begin, end, header in nal_units(au):
        nal = au[begin:end]
        if header & 0x1F == 7:
            start = nal.index(b"\x00\x00\x01") + 3
            bits = "".join("{:08b}".format(b) for b in unescape(nal[start + 1:]))
            at = bits.find("{:032b}{:032b}".format(1001, 60000))
            bits = bits[:at - 1] + "0" + bits[at + 65:]
            bits = bits.rstrip("0")
            bits += "0" * (-len(bits) % 8)
            rbsp = bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))
            nal = nal[:start + 1] + escape(rbsp)
        out += nal
    return bytes(out)


def scte20_entries(cc_data, numbers=(1, 2)):
    """The two 608 constructs of `cc_data` as SCTE 20 pairs of `numbers`."""
    return [(number, cc_data[3 * k + 1], cc_data[3 * k + 2]) for k, number in enumerate(numbers)]


def ts_presentation_ranks(ts):
    """The presentation rank of each video PES (PID 0x100) of an ffmpeg-muxed TS."""
    pts = []
    for i in range(0, len(ts), 188):
        p = ts[i:i + 188]
        pid = ((p[1] & 0x1F) << 8) | p[2]
        if pid == 0x100 and p[1] & 0x40:
            offset = 5 + p[4] if (p[3] >> 4) & 2 else 4
            h = p[offset:]
            b = h[9:14]
            pts.append(((b[0] >> 1) & 7) << 30 | b[1] << 22 | (b[2] >> 1) << 15 | b[3] << 7 | b[4] >> 1)
    ranked = sorted(range(len(pts)), key=lambda k: pts[k])
    ranks = [0] * len(pts)
    for rank, k in enumerate(ranked):
        ranks[k] = rank
    return ranks


def sei_messages_t35(cc_data, split=False):
    """T.35 SEI payloads for `cc_data`: one message, or (`split`) the
    constructs over two ATSC messages beside a foreign and an unprocessed one."""
    if not split:
        return [b"\xb5\x00\x31" + a53_user_data(cc_data)]
    half = len(cc_data) // 6 * 3
    unprocessed = b"\xb5\x00\x31GA94\x03" + bytes([0x01, 0xFF]) + cc_data[:3] + b"\xff"
    foreign = b"\xb5\x00\x2fGA94\x03" + bytes([0x41, 0xFF]) + cc_data[:3] + b"\xff"
    return [b"\xb5\x00\x31" + a53_user_data(cc_data[:half]), foreign, unprocessed,
            b"\xb5\x00\x31" + a53_user_data(cc_data[half:])]


def sei_nal(payloads, hevc, suffix=False):
    message = b""
    for payload in payloads:
        size = len(payload)
        message += bytes([4]) + b"\xff" * (size // 255) + bytes([size % 255]) + payload
    header = (b"\x50\x01" if suffix else b"\x4e\x01") if hevc else b"\x06"
    return b"\x00\x00\x00\x01" + header + escape(message + b"\x80")


def build():
    if os.path.isdir(OUT):
        shutil.rmtree(OUT)
    os.makedirs(OUT)
    cases = []
    tmp = os.path.join(OUT, "tmp")
    os.makedirs(tmp)

    # MPEG-2: A/53 and SCTE 20 user data, starting at an open GOP (raw and TS)
    # and at a closed one; SCTE 20 field numbers with the top field first.
    base_path = os.path.join(tmp, "base.m2v")
    encode("mpeg2", 300, base_path)
    full = open(base_path, "rb").read()
    open_gop = mpeg2_from_second_gop(full)

    def mpeg2_case(name, es, user_data, ts=True):
        order = mpeg2_display_order(es)
        payloads = [user_data(rank) for rank in order]
        path = write(name + ".m2v", mpeg2_insert(es, payloads))
        cases.append((name + "-m2v", path, ["cc"]))
        if ts:
            remux(path, os.path.join(OUT, name + ".ts"), "mpegvideo")
            cases.append((name + "-ts", os.path.join(OUT, name + ".ts"), ["cc"]))

    data = caption_frames(400)
    mpeg2_case("mpeg2-a53", open_gop, lambda n: [a53_user_data(data[n])])
    mpeg2_case("mpeg2-a53-closed", full, lambda n: [a53_user_data(data[n])], ts=False)
    mpeg2_case("mpeg2-scte20", open_gop, lambda n: [scte20_user_data(scte20_entries(data[n]))])
    mpeg2_case("mpeg2-scte20-tff", mpeg2_set_top_field_first(open_gop),
               lambda n: [scte20_user_data(scte20_entries(data[n], (3, 2) if n % 5 else (1, 0)))])
    # The first caption format locks: A/53 here, so later SCTE 20 is ignored; an
    # unprocessed A/53 block and one announcing more constructs than it holds.
    unprocessed = b"GA94\x03\x02\xff\xfc\x94\x2c\xff"
    overlong = b"GA94\x03\x5f\xff\xfc\x94\x2c"
    mpeg2_case("mpeg2-mixed", open_gop,
               lambda n: ([unprocessed, a53_user_data(data[n]), scte20_user_data(scte20_entries(data[n]))]
                          if n % 7 else [overlong]), ts=False)

    # SCTE 20 user data announcing no pairs does not lock the format.
    mpeg2_case("mpeg2-scte20-empty", open_gop,
               lambda n: [scte20_user_data([])] if n < 30 else [a53_user_data(data[n])], ts=False)

    # More caption bytes than FFmpeg keeps for one picture (6000): blocks of
    # padding constructs before the caption data. A block that would pass the
    # limit is left out (the caption data when n % 4 == 0, a padding block when
    # n % 4 == 2), a later one that fits is kept, and the data reaching exactly
    # 6000 bytes (n % 4 == 1) is kept. The leading B pictures' bytes pass on to
    # the next picture, which keeps none of its own.
    def padding(count):
        return a53_user_data(bytes([0xFA, 0x00, 0x00]) * count)

    capped = {0: [padding(7)], 1: [padding(6)], 2: [padding(20)], 3: None}
    mpeg2_case("mpeg2-a53-limit", open_gop,
               lambda n: ([padding(31)] * 64 + capped[n % 4] if capped[n % 4] else [])
               + [a53_user_data(data[n])])

    # DVD and Dish Network caption user data; a stream locked to DVD ignores
    # the A/53 data that follows.
    mpeg2_case("mpeg2-dvd", open_gop, lambda n: [dvd_user_data(data[n], n % 4 != 3, 15 if n % 3 else 1)])
    mpeg2_case("mpeg2-dish", open_gop, lambda n: [dish_user_data(data[n], n)])
    mpeg2_case("mpeg2-dvd-lock", full,
               lambda n: [dvd_user_data(data[n])] if n < 60 else [a53_user_data(data[n])], ts=False)

    # Display durations: repeated fields in an interlaced and a progressive
    # sequence; field pictures.
    mpeg2_case("mpeg2-rff", mpeg2_set_display(full, False, lambda n: (n % 2, n % 3 == 1, n % 4 != 3)),
               lambda n: [a53_user_data(data[n])])
    mpeg2_case("mpeg2-rff-progressive", mpeg2_set_display(full, True, lambda n: (n % 2, n % 3 == 0, 1)),
               lambda n: [a53_user_data(data[n])])
    fields = mpeg2_field_pictures(full)
    field_data = caption_frames(800)
    rank = mpeg2_display_order(fields)
    payloads = [[a53_user_data(field_data[2 * r + k % 2])] for k, r in enumerate(rank)]
    path = write("mpeg2-fields.m2v", mpeg2_insert(fields, payloads))
    cases.append(("mpeg2-fields-m2v", path, ["cc"]))

    # H.264 and HEVC in TS, Matroska and MP4; DTVCC only; field 2 first; split SEI.
    for codec, hevc in (("h264", False), ("hevc", True)):
        source = os.path.join(tmp, codec + ".ts")
        encode(codec, 300, source)
        base = open(source, "rb").read()
        ranks = ts_presentation_ranks(base)

        def nal_case(name, cc, split=False, containers=(".ts", ".mkv", ".mp4")):
            ts = ts_rewrite_video(base, lambda n, au: annexb_insert(
                au, [sei_nal(sei_messages_t35(cc[ranks[n]], split), hevc)], hevc))
            path = write(name + ".ts", ts)
            for ext in containers:
                out = path if ext == ".ts" else os.path.join(OUT, name + ext)
                if ext != ".ts":
                    remux(path, out)
                cases.append(("%s-%s" % (name, ext[1:]), out, ["cc"]))
            if ".ts" not in containers:
                os.remove(path)

        nal_case(codec + "-a53", caption_frames(400))
        if not hevc:
            nal_case("h264-field2", caption_frames(400, field2_first=True), containers=(".mkv",))
            nal_case("h264-split", caption_frames(400), split=True, containers=(".mp4",))
            dtvcc = frames(400, [], [], dtvcc_packets(40))
            nal_case("h264-dtvcc", dtvcc, containers=(".mkv",))
            # Missing frames: the frame period comes from the SPS timing.
            data = caption_frames(400)
            ts = ts_rewrite_video(base, lambda n, au: None if n % 3 else annexb_insert(
                au, [sei_nal(sei_messages_t35(data[ranks[n]]), False)], False))
            cases.append(("h264-gaps-ts", write("h264-gaps.ts", ts), ["cc"]))
            # No VUI timing: the period is the least decode time step.
            ts = ts_rewrite_video(base, lambda n, au: None if n % 7 == 2 else annexb_insert(
                strip_vui_timing(au), [sei_nal(sei_messages_t35(data[ranks[n]]), False)], False))
            cases.append(("h264-novui-ts", write("h264-novui.ts", ts), ["cc"]))
        # A different caption stream for the second video track of the two-track files.
        second = caption_frames(400, field2_first=True)
        ts = ts_rewrite_video(base, lambda n, au: annexb_insert(
            au, [sei_nal(sei_messages_t35(second[ranks[n]]), hevc)], hevc))
        write(codec + "-second.ts", ts)

        # Starts inside a stream with open GOPs.
        source = os.path.join(tmp, codec + "-open.ts")
        encode(codec, 300, source, open_gop=True)
        open_base = open(source, "rb").read()
        open_ranks = ts_presentation_ranks(open_base)
        data = caption_frames(600)

        def cut_case(name, first, strip=False, suffix=False, containers=(".ts",)):
            def transform(n, au):
                if n < first:
                    return None
                if strip:
                    au = strip_recovery_points(au)
                sei = sei_nal(sei_messages_t35(data[open_ranks[n]]), hevc)
                au = annexb_insert(au, [sei], hevc)
                if suffix:
                    au += sei_nal(sei_messages_t35(data[open_ranks[n] + 200]), hevc, suffix=True)
                return au
            path = write(name + ".ts", ts_rewrite_video(open_base, transform))
            for ext in containers:
                out = path if ext == ".ts" else os.path.join(OUT, name + ext)
                if ext != ".ts":
                    remux(path, out)
                cases.append(("%s-%s" % (name, ext[1:]), out, ["cc"]))

        if hevc:
            cut_case("hevc-cra", 10, containers=(".ts", ".mkv"))
            # A reserved VCL unit (type 10) before the slice of the second CRA
            # picture: the decoder skips it, so the picture is still a CRA picture
            # and its RASL pictures are output.
            cra = open(os.path.join(OUT, "hevc-cra.ts"), "rb").read()
            ts = ts_rewrite_video(cra, lambda n, au: au if n != 12 else reserved_before_slice(au))
            cases.append(("hevc-reserved-ts", write("hevc-reserved.ts", ts), ["cc"]))
            # An end of sequence first in the second CRA access unit and an end
            # of bitstream last in the one before the third. Each CRA picture
            # then starts a new sequence: it discards the picture still waiting
            # for output, and its RASL pictures are not decoded and pass their
            # captions to the next picture decoded.
            ends = {12: (b"\0\0\x01\x48\x01", b""), 23: (b"", b"\0\0\x01\x4a\x01")}
            ts = ts_rewrite_video(cra, lambda n, au: ends.get(n, (b"", b""))[0] + au + ends.get(n, (b"", b""))[1])
            cases.append(("hevc-eos-ts", write("hevc-eos.ts", ts), ["cc"]))
            cut_case("hevc-trail", 13)
            cut_case("hevc-suffix", 0, suffix=True, containers=(".ts", ".mp4"))
        else:
            cut_case("h264-recovery", 10, containers=(".ts", ".mkv"))
            cut_case("h264-norecovery", 10, strip=True)
            cut_case("h264-p", 13)

    # Two video tracks (H.264, then HEVC with other captions); Matroska with a
    # TimestampScale of 100 us.
    for ext in (".mkv", ".mp4"):
        path = os.path.join(OUT, "two-video" + ext)
        run([FFMPEG, "-v", "error", "-y", "-fflags", "+bitexact", "-i", os.path.join(OUT, "h264-a53.ts"),
             "-i", os.path.join(OUT, "hevc-second.ts"), "-map", "0:v", "-map", "1:v", "-c", "copy",
             "-flags", "+bitexact", path])
        for stream in (0, 1):
            cases.append(("two-video-%s-stream%d" % (ext[1:], stream), path, ["cc", "stream=%d" % stream]))
    for name in ("h264-second.ts", "hevc-second.ts"):
        os.remove(os.path.join(OUT, name))
    mkv = bytearray(open(os.path.join(OUT, "h264-a53.mkv"), "rb").read())
    at = mkv.find(bytes([0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]))
    mkv[at + 4:at + 7] = bytes([0x01, 0x86, 0xA0])
    cases.append(("h264-timestamp-scale-mkv", write("h264-timestamp-scale.mkv", bytes(mkv)), ["cc"]))
    shutil.rmtree(tmp)

    # SCC: the CC1 pop-on script and the roll-up script on field 1, one pair a frame.
    for name, script in (("scc-popon", popon_script(1)), ("scc-rollup", rollup_script(1))):
        lines = []
        frame = 30
        for pair in script:
            if pair is not None:
                lines.append((frame, [pair]))
            frame += 1
        merged = []
        for f, pairs in lines:
            if merged and merged[-1][0] + len(merged[-1][1]) == f and len(merged[-1][1]) < 32:
                merged[-1][1].extend(pairs)
            else:
                merged.append((f, list(pairs)))
        cases.append((name, scc(name + ".scc", merged), ["cc", "source=packets"]))

    # Picture output departures from H.264 Annex C.4 and H.265 Annex C.5.2,
    # each named by the extraction rule it pins.
    cases.extend(picture_output_cases(tmp_dir=os.path.join(OUT, "tmp")))
    return cases


def picture_output_cases(tmp_dir):
    os.makedirs(tmp_dir, exist_ok=True)
    out = []
    data = caption_frames(400)

    def captioned(name, ts, hevc, transform=None):
        """`ts` with each access unit through transform(n, au) (None drops
        it), then given the caption data of its presentation rank."""
        if transform is not None:
            ts = ts_rewrite_video(ts, transform)
        ranks = ts_presentation_ranks(ts)
        ts = ts_rewrite_video(ts, lambda n, au: annexb_insert(
            au, [sei_nal(sei_messages_t35(data[ranks[n]]), hevc)], hevc))
        out.append((name + "-ts", write(name + ".ts", ts), ["cc"]))

    def base(codec, name, frames=300, **options):
        path = os.path.join(tmp_dir, name + ".ts")
        encode(codec, frames, path, **options)
        return open(path, "rb").read()

    eos = b"\x00\x00\x00\x01\x48\x01"
    hevc_open = base("hevc", "hevc-open", open_gop=True)
    # CRA pictures stand at access units 10, 22, 34, ... (every 12th after
    # the IDR picture); the edits below repeat every 48 access units.

    # HEVC: the buffer makes room at sps_max_dec_pic_buffering_minus1 + 2
    # pictures, before the reference picture set applies; a buffer below the
    # reorder count is raised to it (with an end of sequence before CRA
    # pictures, whose discards show the pictures waiting).
    captioned("hevc-dpb", hevc_open, True, lambda n, au: hevc_set_ordering(au, 2, 2, 0))
    captioned("hevc-dpb-raise", hevc_open, True,
              lambda n, au: (eos if n % 48 == 22 else b"") + hevc_set_ordering(au, 1, 3, 0))
    # An end of sequence before a RASL picture of a CRA picture that is not
    # the first: the pictures waiting are output before it is decoded.
    captioned("hevc-eos-rasl", hevc_open, True, lambda n, au: eos + au if n % 48 == 23 else au)
    # VUI timing changing at CRA pictures: each starts a new sequence.
    captioned("hevc-sps-change", hevc_open, True, lambda n, au: set_vui_timing(
        au, (1001, 30000), (1001, 240000), True) if (n + 26) // 48 % 2 else au)
    # The SPS repeated with a copy as id 1 before every CRA picture, the copy
    # changed (in its latency limit, which changes no order) every other 48
    # access units and the SPS itself for 48 of them: only the changes of the
    # SPS the CRA picture activates start a new sequence (probe p94).
    def sps_ids(n, au):
        def change(nal):
            if (nal[0] >> 1) & 0x3F != 33:
                return nal
            one = hevc_sps_id(nal, 1)
            if (n + 26) // 48 % 2:
                one = hevc_sps_latency(one, 1000)
            if (n + 26) // 48 == 3:
                nal = hevc_sps_latency(nal, 1000)
            return nal + b"\x00\x00\x00\x01" + one
        return map_nals(au, change)
    captioned("hevc-sps-ids", hevc_open, True, sps_ids)

    pyramid = "bframes=3:b-adapt=0:b-pyramid=strict"
    h264_pyr = base("h264", "h264-pyr", params=pyramid)
    h264_pyr_open = base("h264", "h264-pyr-open", open_gop=True, params=pyramid)
    h264_closed = base("h264", "h264-closed")
    # The reorder depth of a bitstream restriction (1, where the pyramid needs
    # 2): pictures that would leave after a later one are dropped.
    captioned("h264-reorder", avc_edit(h264_pyr, transform_sps=lambda nal: avc_set_restriction(nal, (1, 2))), False)
    # No bitstream restriction, from a non-IDR I picture without recovery point.
    captioned("h264-norestr", avc_edit(
        h264_pyr_open, transform_sps=lambda nal: avc_set_restriction(nal, None),
        transform_au=lambda n, au: strip_recovery_points(au) if n >= 9 else None), False)
    # No bitstream restriction at 352x288 and level 1.0 (one frame): the depth
    # grows after the first picture dropped.
    cif = base("h264", "h264-cif", params=pyramid, size="352x288")

    def level10(nal):
        nal = bytearray(avc_set_restriction(nal, None))
        nal[3] = 10
        return bytes(nal)
    captioned("h264-grow", avc_edit(cif, transform_sps=level10), False)
    # no_output_of_prior_pics_flag on every fourth IDR picture.
    captioned("h264-noout", avc_edit(h264_closed, transform_slice=lambda n, sl, nal: sl.rewrite(
        nal, marking=lambda ops: True)[0] if n % 48 == 12 else nal), False)
    # memory_management_control_operation 5 on a P picture every 48 access units.
    h264_cavlc = base("h264", "h264-cavlc", params="bframes=2:b-adapt=0:b-pyramid=none:cabac=0:weightp=0")
    captioned("h264-mmco5", avc_edit(h264_cavlc, transform_slice=avc_mmco5(lambda n: n % 48 == 4)), False)
    # From a non-IDR I picture with memory_management_control_operation 5 and
    # without its recovery point SEI.
    h264_cavlc_open = base("h264", "h264-cavlc-open", open_gop=True,
                           params="bframes=2:b-adapt=0:b-pyramid=none:cabac=0:weightp=0")
    captioned("h264-mmco5-start", avc_edit(
        h264_cavlc_open, transform_slice=avc_mmco5(lambda n: n == 10),
        transform_au=lambda n, au: strip_recovery_points(au) if n >= 10 else None), False)
    # Every third picture of an open-GOP stream without B pictures (4-bit
    # frame_num continuing across its non-IDR I pictures, order count type
    # 2): frame_num jumps across its wrap.
    h264_open_nob = base("h264", "h264-open-nob", open_gop=True, params="bframes=0")
    captioned("h264-gap", h264_open_nob, False, lambda n, au: au if n % 3 == 0 else None)
    # From a non-IDR I picture whose recovery point SEI names the next
    # reference picture (recovery_frame_cnt 1).
    captioned("h264-recovery-cnt1", h264_pyr_open, False,
              lambda n, au: set_recovery_frame_cnt(au, 1) if n >= 9 else None)
    # A VUI frame period of 1/1000 s (45/90000 per field), and no VUI timing
    # with decode time steps alternating 4 and 6 frames before every frame.
    captioned("h264-vui-fast", avc_edit(h264_closed, transform_sps=lambda nal: avc_set_restriction(
        nal, (1, 2), timing=(45, 90000))), False)
    sparse = {0}
    at = 0
    for k in range(20):
        at += 4 if k % 2 == 0 else 6
        sparse.add(at)
    h264_nob = base("h264", "h264-nob", params="bframes=0")
    captioned("h264-estimate", h264_nob, False,
              lambda n, au: strip_vui_timing(au) if n in sparse or n > at else None)

    # Directly written 32x32 frames (P frames with order counts 2 apart and
    # non-reference B pictures `above` P frames below the last), carried in
    # the PES packets of a libx264 encode; each picture gets the caption data
    # of its decode index.
    carrier32 = base("h264", "h264-carrier32", params="bframes=0")

    def written(name, pictures, restriction=None, size=(2, 2), level=30, pts=None, gaps=False,
                recovery=None, mkv_ns=None, carrier=carrier32):
        """A case of `pictures` in the carrier's packets (or, with mkv_ns(n)
        times in nanoseconds, in a Matroska file without durations), with a
        recovery point of recovery_frame_cnt `recovery` on the first."""
        def au(n, payload):
            if n >= len(pictures):
                return None
            kind, idr, ref, fn, poc = pictures[n]
            nals = [b"\x09\xf0"] + ([frame_sps(restriction, size, level, gaps), field_pps()] if n == 0 else [])
            units = b"".join(b"\x00\x00\x00\x01" + nal for nal in nals)
            if recovery is not None and n == 0:
                units += recovery_point_sei(recovery)
            sei = sei_nal(sei_messages_t35(data[n % len(data)]), False)
            slice_nal = frame_slice(kind, idr, ref, fn, poc, size[0] * size[1])
            return units + sei + b"\x00\x00\x00\x01" + slice_nal
        if mkv_ns is not None:
            units = [(mkv_ns(n), au(n, b"")) for n in range(len(pictures))]
            out.append((name + "-mkv", write(name + ".mkv", matroska(units)), ["cc"]))
        else:
            out.append((name + "-ts", write(name + ".ts", ts_rewrite_video(carrier, au, pts)), ["cc"]))

    # The start hold and depth growth without a bitstream restriction: a B
    # picture below 5 P frames at decode index 6 is kept (nothing leaves
    # before the seventh picture) and makes the depth 5; later B pictures 5
    # and 6 below are kept and dropped; one 20 below leaves at once.
    written("h264-start-hold", frame_pictures(300, bs={6: 5, 40: 5, 60: 6, 90: 20}))
    # Depth growth after the start: a B picture 3 below at index 30 is late
    # and dropped, the depth becomes 3; at 60 one 3 below is kept, at 80 one
    # 4 below dropped.
    written("h264-depth-grow", frame_pictures(300, bs={30: 3, 60: 3, 80: 4}))
    # The longer start hold: a depth of 1 by the seventh picture holds the
    # pictures until index 8 (a B picture 5 below at 8 is kept and makes the
    # depth 5); a B picture in order (at index 1) makes one picture wait (one
    # 1 below at 30 is kept).
    written("h264-hold-depth1", frame_pictures(300, bs={2: 1, 8: 5, 12: 2}))
    written("h264-b-in-order", frame_pictures(300, bs={1: 0, 30: 1}))
    # Starts without IDR picture or recovery point: P frames, then a non-IDR
    # I frame at index 2; with reorder depth 2 the I frame is kept and the 2
    # frames after it dropped; without a restriction (depth 0) the I frame
    # is dropped and those after it kept.
    written("h264-start-intra-r2", frame_pictures(300, start=2), restriction=(2, 3))
    written("h264-start-intra", frame_pictures(300, start=2))

    # Field pairs in separate packets (the carrier's PES headers keep their
    # times; each field gets the caption data of its frame's display index
    # and its parity).
    fields = field_pictures(150)
    carrier = base("h264", "h264-carrier", params="bframes=0")

    def field_au(n, au):
        if n >= len(fields):
            return None
        nals = [b"\x09\xf0"] + ([field_sps(), field_pps()] if n == 0 else [])
        sei = sei_nal(sei_messages_t35(data[(2 * fields[n][1] + n % 2) % len(data)]), False)
        units = b"".join(b"\x00\x00\x00\x01" + nal for nal in nals)
        return units + sei + b"\x00\x00\x00\x01" + fields[n][0]
    out.append(("h264-fields-ts", write("h264-fields.ts", ts_rewrite_video(carrier, field_au)), ["cc"]))
    out += hold_and_field_cases(written, carrier, fields, data)
    rate_cases(written)
    output_rule_cases(written)
    # Packets without a time (probes p93, p93b): none at all, where every
    # frame lasts one tick; and none for the first 4300, where the estimate
    # settles after 4096 packets with no time (one tick) and the reference
    # takes the times after them (3003; open).
    written("h264-untimed", frame_pictures(300), pts=lambda n: None)
    long_carrier = base("h264", "h264-carrier-long", frames=4400, params="bframes=0")
    written("h264-untimed-far", frame_pictures(4400), carrier=long_carrier,
            pts=lambda n: None if n < 4300 else 90000 + 3003 * n)
    shutil.rmtree(tmp_dir)
    return out


def rate_cases(written):
    """The duration estimate without VUI timing: 300 P frames (one jump where
    named) whose PTS follow the probes' patterns (probes p79-p83)."""
    def at_rate(p, q, jitter=True):
        # Frames at p/q per second, a tick added on odd frames so that the
        # steps share no divisor.
        return lambda n: 90000 + (2 * n * 90000 * q + p) // (2 * p) + (n % 2 if jitter else 0)

    plain = frame_pictures(300)
    # One of each kind of candidate rate: twelfths (29 + 11/12 fps), whole
    # (45), fast (240) and NTSC (24000/1001).
    written("h264-rate-twelfths", plain, pts=at_rate(359, 12))
    written("h264-rate-whole", plain, pts=at_rate(45, 1))
    written("h264-rate-fast", plain, pts=at_rate(240, 1))
    written("h264-rate-ntsc", plain, pts=at_rate(24000, 1001))
    # 11/12 fps is no candidate: its double, 22/12, fits as well (49090);
    # 70 fps fits none, nor a multiple: one tick.
    written("h264-rate-multiple", plain, pts=at_rate(11, 12))
    written("h264-rate-fallback", plain, pts=at_rate(70, 1))
    # Steps of 3100 share a divisor above 1/500 s: 3100, though 29 fps fits.
    written("h264-rate-divisor", plain, pts=lambda n: 90000 + 3100 * n)
    # 12.5 fps with five steps of one tick among the first 20: the mean step
    # is too short for 12.5 fps (below 0.8 of its rate), so 25 (3600).
    short = {0, 5, 9, 13, 17}
    times = [90000]
    for k in range(300):
        times.append(times[-1] + (1 if k in short else 7200 + (1 if k % 2 else -1)))
    written("h264-rate-skip", plain, pts=lambda n: times[n])
    # Every other frame 719 ticks late at 25 fps scores just under the
    # threshold (3600); 721 just over (one tick).
    written("h264-rate-close", plain, pts=lambda n: 90000 + 3600 * n + 719 * (n % 2))
    written("h264-rate-far", plain, pts=lambda n: 90000 + 3600 * n + 721 * (n % 2))
    # An order count jump at frame 1 leaves frames 2-6 out of the estimate:
    # frame 2 moved 1501 later does not count (3003, not 1501).
    jump = frame_pictures(300, jumps={1})
    written("h264-rate-jump", jump, pts=lambda n: at_rate(30000, 1001)(n) + (1501 if n == 2 else 0))


def output_rule_cases(written):
    """Probe streams of the H.264 output rules: far-back pictures, frame_num
    gaps with gaps_in_frame_num_value_allowed_flag, a stated depth raised by
    a B picture, starts without an IDR picture (intra runs, order count
    jumps in the start hold, a B picture among the first 21 pictures, an
    intra picture with frame_num 0, a non-reference intra picture, a
    recovery point above the intra picture), the start hold's step, the depth
    growth, a stated depth against a jump, and the duration estimate of two
    packets; and two open behaviours of the prepared packets' durations."""
    def shifted(pictures, by, start=1):
        return [(k, i, r, f, q + by if n >= start else q) for n, (k, i, r, f, q) in enumerate(pictures)]

    # A B picture below each of the last 15 pictures but not the 16th last is
    # dropped as late; one below each of the last 16 leaves (probe p87).
    written("h264-far-back", frame_pictures(300, bs={36: 15, 60: 16}))
    # With gaps_in_frame_num_value_allowed_flag set, the step of 4 after a
    # frame_num gap still raises the depth: the B picture at 31 is kept
    # (dropped in h264-gap-jump; probe p88c).
    written("h264-gap-allowed", frame_pictures(300, bs={31: 1, 40: 1}, jumps={30}, gaps={30}), gaps=True)
    # A stated depth of 0 is raised by a B picture below an earlier one up to
    # index 20: the B picture one below the step at 40 is kept (probe p88).
    written("h264-stated-b20", frame_pictures(300, bs={20: 1, 41: 1}, jumps={40}), restriction=(0, 2))
    # Against a stated depth of 0 a step of 4 raises nothing: the B picture
    # one below it is dropped (probe p74).
    written("h264-jump-stated", frame_pictures(300, bs={31: 1}, jumps={30}), restriction=(0, 2))
    # Without an IDR picture, reorder 2: intra pictures among the first
    # pictures above the first are kept (probe p89 I-k3-r2).
    written("h264-start-intra-run", start_pictures("III"), restriction=(2, 3))
    # No restriction: a step of 4 at index 4, in the start hold, makes the
    # depth 1 when the hold ends, so the first picture above the intra
    # picture is dropped (probe p89 J-j4).
    written("h264-start-jump", start_pictures("I", steps={4: 4}))
    # A B picture at index 14, after the hold: the first picture above the
    # intra picture is dropped all the same (probe p89e N14).
    written("h264-start-b-window", start_pictures("I" + "P" * 13 + "B", steps={14: -1, 15: 3}))
    # The intra picture has frame_num 0: its output does not count in the
    # start hold, so the step at index 7 still falls in it (probe p89c j7-f0).
    written("h264-start-fn0", start_pictures("I", steps={7: 4}, frame_num=0))
    # A non-reference intra picture with frame_num 0 recovers nothing; the
    # reference intra picture at index 5 is the first (probe p91 fn-k5R).
    written("h264-start-nonref", start_pictures("IPPPPI", refs={0: False}, frame_num=0))
    # A recovery point naming the P picture above the intra picture recovers
    # it; the B picture below it still counts towards reorder 2 and is
    # dropped (probe p90 pb-c1-r2).
    pb, anchor, fn = [("I", False, True, 5, 44)], 44, 6
    while len(pb) < 300:
        anchor += 6
        pb += [("P", False, True, fn, anchor), ("B", False, False, fn + 1, anchor - 4),
               ("B", False, False, fn + 1, anchor - 2)]
        fn += 1
    written("h264-recovery-above", pb[:300], restriction=(2, 2), recovery=1)
    # The start hold ends at index 7 after a first step of 3 (above 2), as
    # after one of 4 in h264-hold-jump: the B picture 5 below at 8 is dropped
    # (probe p64).
    written("h264-hold-step3", shifted(frame_pictures(300, bs={2: 1, 8: 5, 12: 2}, jumps={1}), -1))
    # Depth growth from a late B picture 2 below: dropped itself, it makes the
    # depth 2; one 2 below at 40 is kept, one 3 below at 50 dropped (probes
    # p47, p57).
    written("h264-depth-two", frame_pictures(300, bs={25: 2, 40: 2, 50: 3}))
    # Two packets: the estimate is one tick (probe p42).
    written("h264-rate-two", frame_pictures(2))
    # Open: PTS-only with a B picture at index 2, the second packet after it
    # moved half a frame. The reference's estimate leaves out a run of packets
    # after the B picture (its window starts at packet 8; probe p71p h8).
    written("h264-rate-pts-b", frame_pictures(300, bs={2: 1}),
            pts=lambda n: 90000 + (2 * n * 90000 * 1001 + 30000) // 60000 + n % 2 + (1501 if n == 4 else 0))
    # Open: Matroska without block durations, DefaultDuration or VUI timing.
    # The reference gives the first frames a duration of 0 (probes p85, p86).
    written("h264-nodur", frame_pictures(300), mkv_ns=lambda n: 10**9 + n * 40_000_000)


def hold_and_field_cases(written, carrier, fields, data):
    """The start hold by an order count jump and by MaxDpbFrames, the depth
    raised by a jump, frame_num gaps, unpaired fields, field pairs in one
    packet and parameter sets only in a Matroska CodecPrivate."""
    out = []
    # A step of 4 from the IDR picture before the B picture that makes the
    # depth 1 ends the hold at index 7: the B picture 5 below at index 8 is
    # dropped (kept in h264-hold-depth1).
    written("h264-hold-jump", frame_pictures(300, bs={2: 1, 8: 5, 12: 2}, jumps={1}))
    # MaxDpbFrames 2 (198 macroblocks at level 1): the depth reaching 2 at
    # index 3 ends the hold there; the B picture 5 below at index 8 is late.
    written("h264-hold-dpb", frame_pictures(300, bs={3: 2, 8: 5, 30: 2}), size=(22, 9), level=10)
    # After the start window, a step of 4 at index 30 raises the depth to 1
    # for good: B pictures one below at 32 and 40 are kept; the same step
    # after a frame_num gap does not (the B picture at 61 is dropped).
    written("h264-jump-depth", frame_pictures(300, bs={32: 1, 40: 1}, jumps={30}))
    written("h264-gap-jump", frame_pictures(300, bs={31: 1, 40: 1}, jumps={30}, gaps={30}))

    def field_case(name, au_of):
        out.append((name + "-ts", write(name + ".ts", ts_rewrite_video(carrier, au_of)), ["cc"]))

    def field_unit(n, first, restriction=(2, 4)):
        nals = [b"\x09\xf0"] + ([field_sps(restriction), field_pps()] if first else [])
        sei = sei_nal(sei_messages_t35(data[(2 * fields[n][1] + n % 2) % len(data)]), False)
        units = b"".join(b"\x00\x00\x00\x01" + nal for nal in nals)
        return units + sei + b"\x00\x00\x00\x01" + fields[n][0]
    # An unpaired field (the second field of the B frame at fields 8, 9 is
    # left out) is not output; its caption bytes go to the next frame.
    lone = [n for n in range(len(fields)) if n != 9]
    field_case("h264-fields-lone", lambda n, au: field_unit(lone[n], n == 0) if n < len(lone) else None)
    # Both fields of each frame in one packet, with B frames and a reorder
    # depth of 1: frames leave by display order (the P frame after the B
    # frames before it, none waiting for its second field).
    field_case("h264-fields-packed", lambda n, au: field_unit(2 * n, n == 0, (1, 2)) + field_unit(2 * n + 1, False)
               if 2 * n + 1 < len(fields) else None)
    # The H.264 stream of h264-hold-depth1 in Matroska with its parameter
    # sets only in the CodecPrivate.
    source = os.path.join(OUT, "h264-hold-depth1.ts")
    target = os.path.join(OUT, "h264-config.mkv")
    run([FFMPEG, "-hide_banner", "-loglevel", "error", "-y", "-i", source, "-map", "0:v", "-c", "copy",
         "-bsf:v", "filter_units=remove_types=7|8", "-fflags", "+bitexact", target])
    out.append(("h264-config-mkv", target, ["cc"]))
    return out


def fate_cases():
    out = []
    dest = os.path.join(CORPUS, "fate/sub")
    os.makedirs(dest, exist_ok=True)
    for name, args in (("Closedcaption_rollup.m2v", ["cc"]), ("scte20.ts", ["cc"]),
                       ("witch.scc", ["cc", "source=packets"])):
        source = os.path.join(FATE, "sub", name)
        if not os.path.exists(source):
            print("missing FATE sample", source, file=sys.stderr)
            continue
        path = os.path.join(dest, name)
        shutil.copyfile(source, path)
        out.append(("fate-" + os.path.splitext(name)[0].lower().replace("_", "-"), path, args))
    return out


OPTIONS = {
    "": [],
    "-realtime": ["opt:real_time=1"],
    "-first": ["opt:data_field=first"],
    "-second": ["opt:data_field=second"],
}


def manifest(cases):
    entries = []
    for case_id, path, args in cases:
        variants = ["", "-realtime"]
        if path.endswith((".ts", ".mkv", ".mp4", ".m2v")) and "fate" not in case_id:
            variants += ["-first", "-second"]
        if path.endswith(".scc"):
            variants += ["-realtime-latency0"]
        for variant in variants:
            arguments = args + (OPTIONS.get(variant) or (["opt:real_time=1", "opt:real_time_latency_msec=0"]
                                                          if variant == "-realtime-latency0" else []))
            fold = subprocess.run([REFERENCE, "sub-fold", path] + arguments, capture_output=True, text=True)
            expected = None
            if fold.returncode == 0:
                record = json.loads(fold.stdout.strip().splitlines()[-1])
                expected = {k: record[k] for k in ("packets", "bytes", "digest", "input_digest")}
            data = open(path, "rb").read()
            entries.append({
                "id": case_id + variant,
                "profile": case_id,
                "file": os.path.relpath(path, CORPUS),
                "sha256": hashlib.sha256(data).hexdigest(),
                "rights": "fetched-not-redistributed" if case_id.startswith("fate-") else "authored-synthetic",
                "source": "fate-suite" if case_id.startswith("fate-") else "harness/subtitles/cc_streams.py",
                "arguments": arguments,
                "expected": expected,
            })
    os.makedirs(os.path.dirname(MANIFEST), exist_ok=True)
    with open(MANIFEST, "w") as f:
        json.dump({"schema": 1, "cases": entries}, f, indent=2)
        f.write("\n")


def reference_lines(mode, path, arguments):
    out = subprocess.run([REFERENCE, mode, path] + arguments, capture_output=True, text=True,
                         check=True).stdout
    return [json.loads(line) for line in out.splitlines()][1:]


def fixtures():
    """Small fixtures for crates/subtitles/tests, with the reference's results."""
    os.makedirs(FIXTURES, exist_ok=True)
    for name in ("scc-popon", "scc-rollup"):
        source = os.path.join(OUT, name + ".scc")
        shutil.copyfile(source, os.path.join(FIXTURES, name + ".scc"))
        for suffix, options in (("", []), ("-realtime", ["opt:real_time=1"])):
            events = reference_lines("sub-trace", source, ["cc", "source=packets"] + options)
            with open(os.path.join(FIXTURES, name + suffix + ".events"), "w") as f:
                for e in events:
                    fields = [str(e["packet"]), "-" if e["pts"] is None else str(e["pts"]), str(e["end"])]
                    f.write("\t".join(fields + [r["ass"] for r in e["rects"]]) + "\n")
    tmp = os.path.join(FIXTURES, "tmp.m2v")
    encode("mpeg2", 40, tmp)
    es = mpeg2_from_second_gop(open(tmp, "rb").read())
    os.remove(tmp)
    data = caption_frames(60)
    order = mpeg2_display_order(es)
    path = os.path.join(FIXTURES, "mpeg2-open-gop.m2v")
    with open(path, "wb") as f:
        f.write(mpeg2_insert(es, [[a53_user_data(data[n])] for n in order]))
    with open(os.path.join(FIXTURES, "mpeg2-open-gop.packets"), "w") as f:
        for p in reference_lines("sub-input", path, ["cc"]):
            pts = "-" if p["pts"] is None else str(p["pts"])
            f.write("%s\t%d\t%s\n" % (pts, p["duration"], p["data"]))


def main():
    cases = fate_cases() + build()
    if os.path.exists(REFERENCE):
        manifest(cases)
        fixtures()
    else:
        print("reference worker missing; manifest and fixtures not written", file=sys.stderr)


if __name__ == "__main__":
    main()
