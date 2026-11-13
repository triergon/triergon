#!/usr/bin/env python3
"""Deterministic DVB subtitle streams for the subtitle corpus.

Builds ETSI EN 300 743 subtitling segments (display definition, page, region,
CLUT and object data segments, end of display set), pixel-data sub-blocks with
2-, 4- and 8-bit run-length strings and map tables, character-coded and
progressive objects, carries them in PES packets (data identifier 0x20,
subtitle stream 0x00, end marker 0xFF) of an MPEG-2 transport stream whose PMT
signals the services with a subtitling descriptor (0x59). Everything is
Python's standard library, byte for byte the same on every run.

Matroska copies (`S_DVBSUB`) are muxed with Homebrew `ffmpeg` (9.0.2) as an
authoring tool and their hashes recorded in the manifest:

    ffmpeg -v error -y -i NAME.ts -map 0:s -c copy -output_ts_offset 1 \
        -fflags +bitexact NAME.mkv

(the offset keeps the segment duration above zero)

`--recorded` also wraps the recorded DVB codec packets of
`crates/subtitles/tests/explicit-clut-fixtures/*.case` (the explicit-CLUT
display sets) into transport streams, one PES per packet at its recorded PTS.

It also wraps the recorded DVB codec packets of the bitmap performance corpus
(`SMDVB001` envelopes) the same way, and copies the earlier authored DVB
inputs kept in `.research/`, the repository's MPEG-TS DVB test streams and the
FATE sample into the corpus. `--manifest` writes
`harness/features/subtitle-dvb/corpus.json` with the reference worker's
`sub-fold` of every case (`target/harness/ffmpeg-subtitles-worker`).

With `--fixtures` it also writes the codec packets and reference folds of
the small cases to `crates/subtitles/tests/dvb-receiver-fixtures/`.

usage: dvb_streams.py [--recorded] [--manifest [--fixtures]] [OUTPUT_DIR]
       (default: .research/subtitle-corpus/dvb)
"""

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import zlib

# ------------------------------------------------------------------ segments

PCS, RCS, CDS, ODS, DDS, DSS, ACS, EDS, STUFFING = 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x80, 0xFF


def segment(kind, page, data):
    return bytes([0x0F, kind]) + struct.pack(">HH", page, len(data)) + bytes(data)


def dds(page=1, version=0, width=720, height=576, window=None):
    """Display definition: `window` is (x_min, x_max, y_min, y_max)."""
    data = bytes([(version & 15) << 4 | (8 if window else 0)]) + struct.pack(">HH", width - 1, height - 1)
    if window:
        data += struct.pack(">HHHH", *window)
    return segment(DDS, page, data)


def pcs(page=1, timeout=5, version=0, state=0, regions=()):
    """Page composition: `regions` is [(region_id, x, y)]."""
    data = bytes([timeout & 255, (version & 15) << 4 | (state & 3) << 2])
    for rid, x, y in regions:
        data += bytes([rid, 0]) + struct.pack(">HH", x, y)
    return segment(PCS, page, data)


DEPTH_CODE = {2: 1, 4: 2, 8: 3}


def rcs(rid, w, h, page=1, version=0, fill=False, depth=4, clut=0, bg=(0, 0, 0),
        objects=(), compat=None, depth_code=None):
    """Region composition: `objects` is [(object_id, x, y)] or
    [(object_id, x, y, type, fg, bg)]; `bg` is the (8, 4, 2)-bit pixel codes."""
    level = compat if compat is not None else DEPTH_CODE.get(depth, 1)
    code = depth_code if depth_code is not None else DEPTH_CODE[depth]
    data = bytes([rid, (version & 15) << 4 | (8 if fill else 0)])
    data += struct.pack(">HH", w, h)
    data += bytes([(level & 7) << 5 | (code & 7) << 2, clut, bg[0] & 255, (bg[1] & 15) << 4 | (bg[2] & 3) << 2])
    for obj in objects:
        oid, x, y = obj[:3]
        kind = obj[3] if len(obj) > 3 else 0
        provider = obj[6] if len(obj) > 6 else 0
        data += struct.pack(">HHH", oid, (kind & 3) << 14 | (provider & 3) << 12 | (x & 0xFFF), y & 0xFFF)
        if kind in (1, 2):
            data += bytes([obj[4] & 255, obj[5] & 255])
    return segment(RCS, page, data)


def cds(cid, entries, page=1, version=0):
    """CLUT definition: entries are (entry_id, mask, y, cr, cb, t, full) with
    mask bits 4=2-bit, 2=4-bit, 1=8-bit CLUT."""
    data = bytes([cid, (version & 15) << 4 | 0x0F])
    for eid, mask, y, cr, cb, t, full in entries:
        data += bytes([eid, (mask & 7) << 5 | 0x1E | (1 if full else 0)])
        if full:
            data += bytes([y, cr, cb, t])
        else:
            word = (y & 63) << 10 | (cr & 15) << 6 | (cb & 15) << 2 | (t & 3)
            data += struct.pack(">H", word)
    return segment(CDS, page, data)


def ods(oid, top, bottom=b"", page=1, version=0, nonmodifying=False, method=0, pad=None):
    """Pixel-coded object data; an empty `bottom` repeats the top field."""
    head = struct.pack(">H", oid) + bytes([(version & 15) << 4 | (method & 3) << 2 | (2 if nonmodifying else 0)])
    data = head + struct.pack(">HH", len(top), len(bottom)) + top + bottom
    if pad is None:
        pad = (len(top) + len(bottom)) % 2 == 1
    if pad:
        data += b"\x00"
    return segment(ODS, page, data)


def ods_chars(oid, codes, page=1, version=0):
    head = struct.pack(">H", oid) + bytes([(version & 15) << 4 | 1 << 2])
    return segment(ODS, page, head + bytes([len(codes)]) + b"".join(struct.pack(">H", c) for c in codes))


def ods_progressive(oid, w, h, payload, page=1, version=0):
    head = struct.pack(">H", oid) + bytes([(version & 15) << 4 | 2 << 2])
    return segment(ODS, page, head + struct.pack(">HHH", w, h, len(payload)) + payload)


def eds(page=1):
    return segment(EDS, page, b"")


# ------------------------------------------------------------------ pixel data


class Bits:
    def __init__(self):
        self.out = bytearray()
        self.acc = 0
        self.n = 0

    def put(self, value, count):
        for shift in range(count - 1, -1, -1):
            self.acc = self.acc << 1 | ((value >> shift) & 1)
            self.n += 1
            if self.n == 8:
                self.out.append(self.acc)
                self.acc = self.n = 0

    def align(self):
        if self.n:
            self.put(0, 8 - self.n)
        return bytes(self.out)


def runs_of(row):
    out = []
    for p in row:
        if out and out[-1][0] == p:
            out[-1][1] += 1
        else:
            out.append([p, 1])
    return out


def code2(b, colour, n, literal=False):
    while n > 0:
        if literal or (colour != 0 and n <= 2):
            if colour == 0:
                b.put(0, 2), b.put(0, 1), b.put(1, 1)
            else:
                b.put(colour, 2)
            n -= 1
        elif colour == 0 and n == 1:
            b.put(0, 2), b.put(0, 1), b.put(1, 1)
            n -= 1
        elif colour == 0 and n == 2:
            b.put(0, 2), b.put(0, 2), b.put(1, 2)
            n -= 2
        elif 3 <= n <= 10:
            b.put(0, 2), b.put(1, 1), b.put(n - 3, 3), b.put(colour, 2)
            n = 0
        elif 12 <= n <= 27:
            b.put(0, 2), b.put(0, 2), b.put(2, 2), b.put(n - 12, 4), b.put(colour, 2)
            n = 0
        elif n >= 29:
            k = min(n, 284)
            b.put(0, 2), b.put(0, 2), b.put(3, 2), b.put(k - 29, 8), b.put(colour, 2)
            n -= k
        else:  # 11 or 28
            code2(b, colour, n - 1)
            n = 1


def code4(b, colour, n, literal=False):
    while n > 0:
        if literal or (colour != 0 and n <= 3):
            if colour == 0:
                b.put(0, 4), b.put(3, 2), b.put(0, 2)
            else:
                b.put(colour, 4)
            n -= 1
        elif colour == 0 and n == 1:
            b.put(0, 4), b.put(3, 2), b.put(0, 2)
            n -= 1
        elif colour == 0 and n == 2:
            b.put(0, 4), b.put(3, 2), b.put(1, 2)
            n -= 2
        elif colour == 0 and 3 <= n <= 9:
            b.put(0, 4), b.put(0, 1), b.put(n - 2, 3)
            n = 0
        elif 4 <= n <= 7:
            b.put(0, 4), b.put(2, 2), b.put(n - 4, 2), b.put(colour, 4)
            n = 0
        elif 9 <= n <= 24:
            b.put(0, 4), b.put(3, 2), b.put(2, 2), b.put(n - 9, 4), b.put(colour, 4)
            n = 0
        elif n >= 25:
            k = min(n, 280)
            b.put(0, 4), b.put(3, 2), b.put(3, 2), b.put(k - 25, 8), b.put(colour, 4)
            n -= k
        else:  # 8
            code4(b, colour, 7)
            n = 1


def code8(b, colour, n, literal=False):
    while n > 0:
        if literal or (colour != 0 and n <= 2):
            if colour == 0:
                b.put(0, 8), b.put(0, 1), b.put(1, 7)
            else:
                b.put(colour, 8)
            n -= 1
        elif colour == 0:
            k = min(n, 127)
            b.put(0, 8), b.put(0, 1), b.put(k, 7)
            n -= k
        else:
            k = min(n, 127)
            if k < 3:
                code8(b, colour, k, literal=True)
            else:
                b.put(0, 8), b.put(1, 1), b.put(k, 7), b.put(colour, 8)
            n -= k


def string(bits, row, literal=False):
    """One pixel-code string (data type 0x10/0x11/0x12) with its end code."""
    b = Bits()
    for colour, n in runs_of(row):
        {2: code2, 4: code4, 8: code8}[bits](b, colour, n, literal)
    if bits == 2:
        b.put(0, 2), b.put(0, 2), b.put(0, 2)
    elif bits == 4:
        b.put(0, 4), b.put(0, 4)
    else:
        b.put(0, 8), b.put(0, 8)
    return bytes([{2: 0x10, 4: 0x11, 8: 0x12}[bits]]) + b.align()


def map_table(kind, values):
    """0x20 2-to-4, 0x21 2-to-8, 0x22 4-to-8 map table sub-block."""
    if kind == 0x20:
        return bytes([0x20, values[0] << 4 | values[1], values[2] << 4 | values[3]])
    return bytes([kind]) + bytes(values)


def field(bits, rows, literal=False, maps=b"", split=None):
    """Pixel-data sub-block for `rows`, each row a string and an end-of-line
    code. `split` cuts a row into several strings at the given offsets."""
    out = bytearray(maps)
    for row in rows:
        if split:
            at = 0
            for cut in list(split) + [len(row)]:
                if cut > at:
                    out += string(bits, row[at:cut], literal)
                at = cut
        else:
            out += string(bits, row, literal)
        out.append(0xF0)
    return bytes(out)


def obj(oid, bits, rows, page=1, version=0, literal=False, nonmodifying=False, maps=b"", repeat=False, split=None):
    """A pixel-coded object of `rows`; interlaced top/bottom fields."""
    top = field(bits, rows[0::2], literal, maps, split)
    bottom = b"" if repeat else field(bits, rows[1::2], literal, maps, split)
    return ods(oid, top, bottom, page=page, version=version, nonmodifying=nonmodifying)


def pattern(w, h, colours, seed=1):
    """Deterministic rows of runs over `colours`."""
    rows = []
    state = seed
    for y in range(h):
        row = []
        while len(row) < w:
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            colour = colours[(state >> 8) % len(colours)]
            n = 1 + (state >> 16) % 13
            if (state >> 4) % 7 == 0:
                n += 20 + (state >> 20) % 300
            row.extend([colour] * n)
        rows.append(row[:w])
    return rows


def text_like(w, h, fg, outline, bg=0, seed=3):
    """Glyph-like rows: background with outlined vertical strokes."""
    rows = []
    for y in range(h):
        row = [bg] * w
        if 2 <= y < h - 2:
            x = 4 + (seed * 7 + y) % 5
            while x + 6 < w:
                row[x] = outline
                row[x + 1:x + 4] = [fg] * 3
                row[x + 4] = outline
                x += 9 + (x * seed + y) % 5
        rows.append(row)
    return rows


def ycrcb(r, g, b):
    y = 16 + (65.738 * r + 129.057 * g + 25.064 * b) / 256
    cb = 128 + (-37.945 * r - 74.494 * g + 112.439 * b) / 256
    cr = 128 + (112.439 * r - 94.154 * g - 18.285 * b) / 256
    return int(round(y)), int(round(cr)), int(round(cb))


def full(eid, mask, rgb, t=0):
    y, cr, cb = ycrcb(*rgb)
    return (eid, mask, y, cr, cb, t, True)


def reduced(eid, mask, y, cr, cb, t):
    return (eid, mask, y, cr, cb, t, False)


def ramp(mask, count, t=0, base=0):
    """Full-range entries across hues with transparency `t`."""
    out = []
    for i in range(count):
        r = (i * 97 + base) % 256
        g = (i * 53 + 40 + base) % 256
        b = (i * 29 + 80 + base) % 256
        out.append(full(i, mask, (r, g, b), (t + i * 7) % 256 if t else 0))
    return out


# ------------------------------------------------------------------ transport


def pes_data(segments, identifier=True, end=True, stuffing=b""):
    data = (b"\x20\x00" if identifier else b"") + b"".join(segments) + stuffing
    return data + (b"\xff" if end else b"")


def pts_field(pts, marker=0x21):
    pts &= (1 << 33) - 1
    return bytes([marker | ((pts >> 29) & 0x0E), (pts >> 22) & 0xFF, ((pts >> 14) & 0xFE) | 1,
                  (pts >> 7) & 0xFF, ((pts << 1) & 0xFE) | 1])


def pes(payload, pts=None, length=True):
    header = bytes([0x80, 0x80 if pts is not None else 0x00, 5 if pts is not None else 0])
    if pts is not None:
        header += pts_field(pts)
    body = header + payload
    size = len(body) if length else 0
    if size > 0xFFFF:
        size = 0
    return b"\x00\x00\x01\xbd" + struct.pack(">H", size) + body


def crc32_mpeg(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = (crc << 1) ^ 0x04C11DB7 if crc & 0x80000000 else crc << 1
            crc &= 0xFFFFFFFF
    return crc


def section(table, extension, body):
    head = bytes([table]) + struct.pack(">H", 0xB000 | (len(body) + 9)) + struct.pack(">H", extension) + b"\xc1\x00\x00"
    data = head + body
    return data + struct.pack(">I", crc32_mpeg(data))


def subtitling_descriptor(services):
    """services: [(language, type, composition_page, ancillary_page)]."""
    body = b"".join(lang.encode() + bytes([kind]) + struct.pack(">HH", comp, anc)
                    for lang, kind, comp, anc in services)
    return bytes([0x59, len(body)]) + body


class Mux:
    def __init__(self):
        self.out = bytearray()
        self.cc = {}

    def packets(self, pid, payload, unit_start=True):
        first = True
        while first or payload:
            cc = self.cc.get(pid, 0)
            self.cc[pid] = (cc + 1) & 15
            head = bytes([0x47, (0x40 if first and unit_start else 0) | pid >> 8, pid & 0xFF])
            room = 184
            if len(payload) >= room:
                self.out += head + bytes([0x10 | cc]) + payload[:room]
                payload = payload[room:]
            else:
                stuff = room - len(payload)
                if stuff == 1:
                    adaptation = b"\x00"
                else:
                    adaptation = bytes([stuff - 1, 0]) + b"\xff" * (stuff - 2)
                self.out += head + bytes([0x30 | cc]) + adaptation + payload
                payload = b""
            first = False

    def psi(self, pid, data):
        self.packets(pid, b"\x00" + data)


def transport(streams, units, pmt_pid=0x1000, program=1, pcr_pid=None):
    """`streams`: [(pid, stream_type, descriptors)]; `units`: [(pid, pes bytes)]
    in mux order. PAT and PMT once at the start."""
    mux = Mux()
    mux.psi(0, section(0x00, 1, struct.pack(">HH", program, 0xE000 | pmt_pid)))
    es = b""
    for pid, kind, descriptors in streams:
        es += bytes([kind]) + struct.pack(">HH", 0xE000 | pid, 0xF000 | len(descriptors)) + descriptors
    pcr = pcr_pid if pcr_pid is not None else streams[0][0]
    mux.psi(pmt_pid, section(0x02, program, struct.pack(">HH", 0xE000 | pcr, 0xF000) + es))
    for pid, data in units:
        mux.packets(pid, data)
    return bytes(mux.out)


SUB_PID = 0x100


def single(sets, services=(("eng", 0x10, 1, 2),), pid=SUB_PID):
    """A one-stream TS. `sets` are (pts, pes_data) or (pts, pes_data, length_flag)."""
    units = []
    for item in sets:
        pts, data = item[0], item[1]
        length = item[2] if len(item) > 2 else True
        units.append((pid, pes(data, pts, length)))
    return transport([(pid, 0x06, subtitling_descriptor(services))], units)


# ------------------------------------------------------------------ cases

T0 = 90000


def basic_region(depth, w=24, h=8, rid=0, clut=1, x=0, y=0, fill=False, bg=(0, 0, 0), objects=((1, 0, 0),), **kw):
    return rcs(rid, w, h, depth=depth, clut=clut, fill=fill, bg=bg, objects=objects, **kw)


def simple_set(depth, rows, clut_entries, pts=T0, page=1, timeout=5, version=0, extra=(), x=40, y=500, literal=False,
               window=None, dds_size=None, nonmodifying=False, maps=b""):
    h = len(rows)
    w = len(rows[0])
    mask = {2: 4, 4: 2, 8: 1}[depth]
    segs = []
    if dds_size or window:
        width, height = dds_size or (720, 576)
        segs.append(dds(page, 0, width, height, window))
    segs += [pcs(page, timeout, version, 0, [(0, x, y)]),
             rcs(0, w, h, page=page, depth=depth, clut=1, objects=[(1, 0, 0)]),
             cds(1, [e[:1] + (mask,) + e[2:] for e in clut_entries], page=page),
             obj(1, depth if not maps else {0x20: 2, 0x21: 2, 0x22: 4}[maps[0]], rows, page=page, literal=literal,
                 nonmodifying=nonmodifying, maps=maps)]
    segs += list(extra)
    segs.append(eds(page))
    return (pts, pes_data(segs))


def clear_set(pts, version, page=1, timeout=5):
    return (pts, pes_data([pcs(page, timeout, version, 0, []), eds(page)]))


def cases():
    out = {}
    four = [full(i, 2, (i * 16, 255 - i * 16, (i * 40) % 256), 0 if i else 255) for i in range(16)]
    two = [full(0, 4, (0, 0, 0), 255), full(1, 4, (255, 255, 255)), full(2, 4, (0, 0, 0)), full(3, 4, (128, 128, 128), 64)]
    eight = ramp(1, 256, t=0)

    # Depths with explicit CLUTs, run and literal codings.
    for depth, entries, colours in ((2, two, [0, 1, 2, 3]), (4, four, list(range(16))), (8, eight, list(range(0, 256, 3)))):
        rows = pattern(64, 12, colours, seed=depth)
        out["depth%d-runs" % depth] = single([simple_set(depth, rows, entries)])
        out["depth%d-literals" % depth] = single([simple_set(depth, rows, entries, literal=True)])
    # Long runs across every run-length form, odd widths.
    for depth, entries in ((2, two), (4, four), (8, eight)):
        top = depth == 2 and 3 or depth == 4 and 15 or 255
        rows = [[0] * n + [top] * (300 - n) for n in (1, 2, 3, 7, 8, 11, 12, 27, 28, 29, 200, 284, 285, 299)]
        rows = [r[:301] + [1] for r in rows]
        out["depth%d-long-runs" % depth] = single([simple_set(depth, rows, entries)])

    # Reduced-resolution CLUT entries and transparency.
    reduced4 = [reduced(i, 2, (i * 4) & 63, i, 15 - i, i & 3) for i in range(16)]
    out["clut-reduced"] = single([simple_set(4, pattern(32, 6, list(range(16))), reduced4)])
    out["clut-y-zero"] = single([simple_set(4, pattern(32, 6, list(range(16))),
                                            [(i, 2, 0 if i % 3 == 0 else 100, 90, 160, i * 9, True) for i in range(16)])])
    out["clut-extremes"] = single([simple_set(8, pattern(64, 4, list(range(256))),
                                              [(i, 1, (i * 37) & 255, (i * 101) & 255, (i * 59) & 255, (i * 13) & 255, True)
                                               for i in range(256)])])

    # Default CLUTs (no CDS for the region's CLUT id) at each depth.
    for depth in (2, 4, 8):
        colours = list(range(1 << depth)) if depth < 8 else list(range(0, 256, 5))
        rows = pattern(48, 10, colours, seed=11 + depth)
        segs = [pcs(1, 5, 0, 0, [(0, 100, 400)]),
                rcs(0, 48, 10, depth=depth, clut=7, objects=[(1, 0, 0)]),
                obj(1, depth, rows), eds()]
        out["default-clut%d" % depth] = single([(T0, pes_data(segs))])
        # Text-like content for the automatic palette.
        tl = text_like(60, 14, fg=1, outline=2 if depth == 2 else 3, bg=0)
        segs = [pcs(1, 5, 0, 0, [(0, 100, 400)]),
                rcs(0, 60, 14, depth=depth, clut=7, objects=[(1, 0, 0)]),
                obj(1, depth, tl), eds()]
        out["default-clut%d-text" % depth] = single([(T0, pes_data(segs))])
    # Default CLUT with few colours, one colour, all background.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 16, 4, depth=4, clut=9, objects=[(1, 0, 0)]),
            obj(1, 4, [[5] * 16] * 4), eds()]
    out["default-clut-one-colour"] = single([(T0, pes_data(segs))])
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 16, 4, depth=4, clut=9, objects=[(1, 0, 0)]),
            obj(1, 4, [[0] * 8 + [9] * 8, [9] * 8 + [0] * 8] * 2), eds()]
    out["default-clut-two-colours"] = single([(T0, pes_data(segs))])
    # CLUT partially defined: some entries explicit.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 32, 6, depth=4, clut=1, objects=[(1, 0, 0)]),
            cds(1, [full(3, 2, (200, 20, 20)), full(5, 2, (20, 200, 20), 128)]),
            obj(1, 4, pattern(32, 6, list(range(16)))), eds()]
    out["clut-partial"] = single([(T0, pes_data(segs))])
    # One CDS entry for several depths at once and for another depth only.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 40)]),
            rcs(0, 16, 4, depth=2, clut=1, objects=[(1, 0, 0)]),
            rcs(1, 16, 4, depth=8, clut=1, objects=[(2, 0, 0)]),
            cds(1, [full(1, 7, (200, 20, 20)), full(2, 1, (20, 200, 20)), full(3, 4, (20, 20, 200), 30)]),
            obj(1, 2, pattern(16, 4, [0, 1, 2, 3])), obj(2, 8, pattern(16, 4, [0, 1, 2, 3])), eds()]
    out["clut-multi-depth"] = single([(T0, pes_data(segs))])

    # Map tables: default and explicit, for each lower-depth string.
    for name, depth, bits, maps in (("map2to4", 4, 2, b""), ("map2to8", 8, 2, b""), ("map4to8", 8, 4, b""),
                                    ("map2to4-table", 4, 2, map_table(0x20, [3, 9, 12, 6])),
                                    ("map2to8-table", 8, 2, map_table(0x21, [5, 0x40, 0x81, 0xfe])),
                                    ("map4to8-table", 8, 4, map_table(0x22, [(i * 17 + 3) & 255 for i in range(16)]))):
        rows = pattern(40, 8, list(range(1 << bits)), seed=len(name))
        entries = four if depth == 4 else eight
        segs = [pcs(1, 5, 0, 0, [(0, 60, 300)]), rcs(0, 40, 8, depth=depth, clut=1, objects=[(1, 0, 0)]),
                cds(1, entries), obj(1, bits, rows, maps=maps), eds()]
        out[name] = single([(T0, pes_data(segs))])
    # A higher-depth string in a lower-depth region and an 8-bit string in 2-bit.
    for name, depth, bits in (("string8-in-region4", 4, 8), ("string4-in-region2", 2, 4), ("string8-in-region2", 2, 8)):
        rows = pattern(24, 4, [0, 1, 2, 3, 17, 34, 200], seed=5)
        rows = [[p & ((1 << bits) - 1) for p in r] for r in rows]
        segs = [pcs(1, 5, 0, 0, [(0, 60, 300)]), rcs(0, 24, 4, depth=depth, clut=1, objects=[(1, 0, 0)]),
                cds(1, two if depth == 2 else four), obj(1, bits, rows), eds()]
        out[name] = single([(T0, pes_data(segs))])
    # Map table changed mid-object.
    top = map_table(0x20, [1, 2, 3, 4]) + string(2, [0, 1, 2, 3] * 4) + b"\xf0" + map_table(0x20, [15, 14, 13, 12]) + \
        string(2, [0, 1, 2, 3] * 4) + b"\xf0"
    segs = [pcs(1, 5, 0, 0, [(0, 60, 300)]), rcs(0, 16, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            cds(1, four), ods(1, top, b""), eds()]
    out["map-table-change"] = single([(T0, pes_data(segs))])

    # Multiple regions, several objects in a region, one object in two regions.
    segs = [pcs(1, 9, 0, 0, [(0, 50, 420), (1, 60, 470), (2, 300, 40)]),
            rcs(0, 120, 20, depth=4, clut=1, objects=[(1, 0, 0), (2, 60, 4)]),
            rcs(1, 80, 16, depth=4, clut=1, objects=[(3, 4, 2), (1, 40, 0)]),
            rcs(2, 30, 10, depth=2, clut=2, objects=[(4, 0, 0)]),
            cds(1, four), cds(2, two),
            obj(1, 4, pattern(30, 12, list(range(16)), 1)),
            obj(2, 4, pattern(50, 14, list(range(16)), 2)),
            obj(3, 4, pattern(20, 10, [0, 5, 9], 3)),
            obj(4, 2, pattern(30, 10, [0, 1, 2, 3], 4)), eds()]
    out["regions-objects"] = single([(T0, pes_data(segs))])
    # PCS order differs from RCS order, regions listed in descending ids.
    segs = [pcs(1, 9, 0, 0, [(5, 300, 40), (2, 50, 420), (9, 60, 470)]),
            rcs(9, 20, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            rcs(2, 20, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            rcs(5, 20, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            cds(1, four), obj(1, 4, pattern(20, 4, list(range(16)), 7)), eds()]
    out["region-order"] = single([(T0, pes_data(segs))])
    # Objects overlapping, clipped at the region edge, positioned outside.
    segs = [pcs(1, 9, 0, 0, [(0, 50, 420)]),
            rcs(0, 40, 12, depth=4, clut=1, objects=[(1, 0, 0), (2, 10, 3), (3, 30, 8), (4, 50, 20)]),
            cds(1, four),
            obj(1, 4, pattern(20, 8, [1, 2, 3], 1)), obj(2, 4, pattern(20, 8, [4, 5, 0], 2)),
            obj(3, 4, pattern(20, 8, [6, 7], 3)), obj(4, 4, pattern(4, 4, [8], 4)), eds()]
    out["objects-overlap-clip"] = single([(T0, pes_data(segs))])
    # Object data before the region composition, object never referenced,
    # region referencing an absent object.
    segs = [obj(1, 4, pattern(20, 4, list(range(16)), 1)),
            pcs(1, 9, 0, 0, [(0, 50, 420), (1, 50, 460)]),
            rcs(0, 20, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            rcs(1, 20, 4, depth=4, clut=1, fill=True, bg=(0, 6, 0), objects=[(7, 0, 0)]),
            cds(1, four), obj(3, 4, pattern(20, 4, [1], 3)), eds()]
    out["object-order"] = single([(T0, pes_data(segs))])

    # Region fill and background codes per depth, region without objects.
    for depth in (2, 4, 8):
        bg = (0x5a, 0x9, 0x2)
        segs = [pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 30)]),
                rcs(0, 16, 6, depth=depth, clut=1, fill=True, bg=bg, objects=[(1, 2, 1)]),
                rcs(1, 8, 3, depth=depth, clut=1, fill=True, bg=bg),
                cds(1, {2: two, 4: four, 8: eight}[depth]),
                obj(1, depth, pattern(10, 4, [0, 1, 2, 3], depth)), eds()]
        out["fill-depth%d" % depth] = single([(T0, pes_data(segs))])
    # Region without fill and without objects; region not in the page.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 8, 3, depth=4, clut=1),
            rcs(3, 8, 3, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
            obj(1, 4, pattern(8, 3, [1, 2], 1)), eds()]
    out["region-empty"] = single([(T0, pes_data(segs))])
    # Compatibility level differing from depth.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 16, 4, depth=8, compat=2, clut=1, objects=[(1, 0, 0)]),
            cds(1, eight), obj(1, 8, pattern(16, 4, [0, 100, 200], 1)), eds()]
    out["region-compat-level"] = single([(T0, pes_data(segs))])
    # Invalid depth code.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 16, 4, depth=4, depth_code=5, clut=1, objects=[(1, 0, 0)]),
            cds(1, four), obj(1, 4, pattern(16, 4, [0, 1, 2], 1)), eds()]
    out["region-depth-invalid"] = single([(T0, pes_data(segs))])

    # Non-modifying colour in each depth over a filled background.
    for depth in (2, 4, 8):
        rows = pattern(20, 6, [0, 1, 2, 1, 3], depth)
        for flag in (False, True):
            segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]),
                    rcs(0, 24, 6, depth=depth, clut=1, fill=True, bg=(7, 7, 2), objects=[(1, 2, 0)]),
                    cds(1, {2: two, 4: four, 8: eight}[depth]),
                    obj(1, depth, rows, nonmodifying=flag), eds()]
            out["nonmodifying-depth%d-%d" % (depth, flag)] = single([(T0, pes_data(segs))])

    # Bottom field repeating the top, odd heights, a missing bottom row.
    rows = pattern(20, 7, list(range(16)), 9)
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 20, 7, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
            obj(1, 4, rows, repeat=True), eds()]
    out["field-repeat"] = single([(T0, pes_data(segs))])
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 20, 8, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
            ods(1, field(4, rows[0::2]), field(4, rows[1::2][:1])), eds()]
    out["field-short-bottom"] = single([(T0, pes_data(segs))])
    # Rows longer than the object's region, a row split over strings.
    rows = pattern(40, 4, list(range(16)), 10)
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 30, 4, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
            obj(1, 4, rows, split=(5, 17)), eds()]
    out["row-split-overflow"] = single([(T0, pes_data(segs))])
    # Rows that run past the region bottom (object taller than region).
    rows = pattern(16, 12, list(range(16)), 12)
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 16, 6, depth=4, clut=1, objects=[(1, 0, 2)]), cds(1, four),
            obj(1, 4, rows), eds()]
    out["rows-past-bottom"] = single([(T0, pes_data(segs))])

    # Display definition: plain, with window, window and SD offsets, too small.
    rows = pattern(32, 6, list(range(16)), 13)
    out["dds-hd"] = single([simple_set(4, rows, four, dds_size=(1920, 1080), x=600, y=900)])
    out["dds-window"] = single([simple_set(4, rows, four, dds_size=(1920, 1080), window=(100, 1819, 50, 1029), x=20, y=30)])
    out["dds-window-sd"] = single([simple_set(4, rows, four, window=(8, 711, 16, 559), x=0, y=0)])
    out["dds-small"] = single([simple_set(4, rows, four, dds_size=(320, 240), x=300, y=200)])
    # DDS changing between display sets, a DDS version repeat.
    sets = [simple_set(4, rows, four, pts=T0, dds_size=(1280, 720), window=(10, 1269, 20, 699)),
            (T0 + 90000, pes_data([dds(1, 1, 720, 576), pcs(1, 5, 1, 0, [(0, 40, 50)]), eds()])),
            (T0 + 180000, pes_data([dds(1, 1, 1920, 1080), pcs(1, 5, 2, 0, [(0, 40, 50)]), eds()]))]
    out["dds-change"] = single(sets)

    # Page time-outs, clears, version repeats, acquisition and mode change.
    rows = pattern(24, 4, list(range(16)), 14)
    sets = [simple_set(4, rows, four, pts=T0, timeout=0),
            clear_set(T0 + 45000, 1, timeout=3),
            simple_set(4, rows, four, pts=T0 + 90000, timeout=255, version=2),
            simple_set(4, rows, four, pts=T0 + 135000, timeout=10, version=2),
            clear_set(T0 + 180000, 3)]
    out["page-timeouts-clear"] = single(sets)
    # A display set that only updates a region (same region version kept).
    r0 = pattern(24, 4, list(range(16)), 15)
    r1 = pattern(24, 4, [0, 3, 6], 16)
    sets = [(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 30)]),
                           rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                           rcs(1, 24, 4, depth=4, clut=1, objects=[(2, 0, 0)]),
                           cds(1, four), obj(1, 4, r0), obj(2, 4, r0), eds()])),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10), (1, 10, 30)]),
                                   obj(2, 4, r1, version=1), eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(1, 10, 30)]),
                                    rcs(1, 24, 4, version=1, depth=4, clut=1, objects=[(2, 0, 0)]), eds()])),
            (T0 + 270000, pes_data([pcs(1, 5, 3, 0, [(1, 12, 30)]),
                                    rcs(1, 24, 4, version=1, depth=4, clut=1, objects=[(2, 0, 0)]),
                                    cds(1, ramp(2, 16, base=90), version=1), eds()]))]
    out["page-update"] = single(sets)
    # Acquisition point and mode change resetting the epoch.
    for state, name in ((1, "acquisition"), (2, "mode-change")):
        sets = [simple_set(4, r0, four, pts=T0),
                (T0 + 90000, pes_data([pcs(1, 5, 1, state, [(0, 10, 10)]), eds()])),
                (T0 + 180000, pes_data([pcs(1, 5, 2, state, [(0, 10, 10)]),
                                        rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                                        obj(1, 4, r1), eds()]))]
        out["page-" + name] = single(sets)
    # Same page version repeated, region resized and re-depthed.
    sets = [simple_set(4, r0, four, pts=T0),
            (T0 + 45000, pes_data([pcs(1, 5, 0, 0, [(0, 300, 300)]), eds()])),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]),
                                   rcs(0, 32, 6, version=1, depth=4, clut=1, objects=[(1, 0, 0)]),
                                   obj(1, 4, pattern(32, 6, [1, 2, 3], 17), version=1), eds()])),
            (T0 + 135000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]),
                                    rcs(0, 32, 6, version=2, depth=8, clut=1, objects=[(1, 0, 0)]),
                                    obj(1, 8, pattern(32, 6, [1, 200, 3], 18), version=2), eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 3, 0, [(0, 10, 10)]),
                                    rcs(0, 32, 6, version=2, depth=8, clut=1, objects=[(1, 0, 0)]),
                                    obj(1, 8, pattern(32, 6, [9, 9, 3], 19), version=3), eds()])),
            (T0 + 225000, pes_data([pcs(1, 5, 4, 0, [(0, 10, 10)]),
                                    rcs(0, 32, 6, version=4, fill=True, bg=(0x33, 2, 1), depth=8, clut=1), eds()]))]
    out["region-versions"] = single(sets)
    # CLUT version repeated with different entries.
    sets = [simple_set(4, r0, four, pts=T0),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]), cds(1, ramp(2, 16, base=10)), eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]), cds(1, ramp(2, 16, base=70), version=1), eds()]))]
    out["clut-versions"] = single(sets)

    # No end of display set: the next page composition, end of stream.
    sets = [(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                           cds(1, four), obj(1, 4, r0)])),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 20, 10)]), eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(0, 30, 10)]), obj(1, 4, r1, version=1)]))]
    out["no-end-of-display"] = single(sets)
    # End of display set twice, end without a page.
    sets = [(T0, pes_data([eds()])),
            (T0 + 1000, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                                  cds(1, four), obj(1, 4, r0), eds(), eds()])),
            (T0 + 90000, pes_data([eds()]))]
    out["end-of-display-repeat"] = single(sets)
    # Two display sets in one PES; a set split over two PES with one PTS.
    a = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
         obj(1, 4, r0), eds()]
    b = [pcs(1, 5, 1, 0, [(0, 50, 10)]), obj(1, 4, r1, version=1), eds()]
    out["two-sets-one-pes"] = single([(T0, pes_data(a + b))])
    whole = b"".join(a)
    out["set-split-pes"] = single([(T0, b"\x20\x00" + whole[:23]), (T0, whole[23:] + b"\xff"),
                                   (T0 + 90000, pes_data(b))])
    out["set-split-pes-new-pts"] = single([(T0, b"\x20\x00" + whole[:23]), (T0 + 10, whole[23:] + b"\xff"),
                                           (T0 + 90000, pes_data(b))])
    # PES without the data identifier, without end marker, with stuffing.
    out["pes-no-identifier"] = single([(T0, pes_data(a, identifier=False)), (T0 + 90000, pes_data(b))])
    out["pes-no-end-marker"] = single([(T0, pes_data(a, end=False)), (T0 + 90000, pes_data(b, end=False))])
    out["pes-stuffing"] = single([(T0, pes_data(a + [segment(STUFFING, 1, b"\xff" * 9)], stuffing=b"\xff" * 5)),
                                  (T0 + 90000, pes_data(b))])
    # PES without a PTS between two with one, and unbounded PES length.
    out["pes-no-pts"] = single([(T0, pes_data(a)), (None, pes_data(b)), (T0 + 180000, pes_data([pcs(1, 5, 2, 0, []), eds()]))])
    out["pes-unbounded"] = single([(T0, pes_data(a), False), (T0 + 90000, pes_data(b), False)])
    # A PES without a PTS reaches the decoder without a time when its data has
    # no identifier and the PES before it ended at its end marker: between
    # two with one, and first.
    def numbered(version):
        return pes_data([pcs(1, 5, version, 0, [(0, 10 + version, 10)]),
                         rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]), cds(1, four),
                         obj(1, 4, r0, version=version), eds()], identifier=version != 1)
    out["no-pts-between"] = single([(T0, numbered(0)), (None, numbered(1)), (T0 + 180000, numbered(2)),
                                    (T0 + 270000, numbered(3))])
    out["no-pts-first"] = single([(None, numbered(1)), (T0, numbered(2)), (T0 + 90000, numbered(3))])
    # The page after a packet without a time builds the display set and its
    # automatic palette without presenting it; a fill after that leaves the
    # palette as computed.
    drawn = pattern(24, 4, [1, 2, 3, 5], 23)
    out["no-pts-palette-then-fill"] = single([
        (T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=7, objects=[(1, 0, 0)]),
                       obj(1, 4, r0), eds()])),
        (None, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]), obj(1, 4, drawn, version=1), eds()], identifier=False)),
        (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]),
                                rcs(0, 24, 4, version=1, fill=True, depth=4, clut=7, bg=(0, 9, 0),
                                    objects=[(1, 0, 0)]), eds()])),
        (T0 + 270000, pes_data([pcs(1, 5, 3, 0, [(0, 10, 10)]), eds()])),
        (T0 + 360000, pes_data([pcs(1, 5, 4, 0, [(0, 10, 10)]), eds()]))])

    # Malformed segments.
    good = b"".join(a)
    out["truncated-segment"] = single([(T0, b"\x20\x00" + good[:-9]), (T0 + 90000, pes_data(b))])
    out["junk-after-segments"] = single([(T0, b"\x20\x00" + good + b"\x0e\x01\x02"), (T0 + 90000, pes_data(b))])
    out["segment-length-overrun"] = single([(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]),
                                                           b"\x0f\x11\x00\x01\x01\x00\x00"]))])
    bad_rcs = segment(RCS, 1, b"\x00\x00\x00\x18")
    bad_pcs = segment(PCS, 1, b"\x05")
    bad_cds = segment(CDS, 1, b"\x01\x00\x03\x20\x10")
    bad_ods = segment(ODS, 1, b"\x00\x01\x00\x00\x10\x00\x02\x11\x00")
    out["short-segments"] = single([(T0, pes_data([bad_pcs, eds()])),
                                    (T0 + 9000, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]), bad_rcs, eds()])),
                                    (T0 + 18000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]),
                                                           rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                                                           bad_cds, bad_ods, eds()])),
                                    (T0 + 27000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]), cds(1, four), obj(1, 4, r0), eds()]))])
    # Object field lengths past the segment, bad data types, no end of string.
    bad1 = ods(1, field(4, r0[0::2]) + b"\x13\x55\x66", field(4, r0[1::2]))
    bad2 = segment(ODS, 1, struct.pack(">HBHH", 1, 0x10, 200, 0) + field(4, r0[0::2]))
    bad3 = ods(1, b"\x11\x12\x34\x56\x78", b"", version=2)
    out["object-malformed"] = single([(T0, pes_data(a[:3] + [bad1, eds()])),
                                      (T0 + 9000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]), bad2, eds()])),
                                      (T0 + 18000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]), bad3, eds()]))])

    # Character-coded and progressive objects.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 40)]),
            rcs(0, 40, 20, depth=4, clut=1, fill=True, bg=(0, 1, 0), objects=[(1, 0, 0, 1, 2, 3)]),
            rcs(1, 40, 8, depth=4, clut=1, objects=[(2, 0, 0)]),
            cds(1, four), ods_chars(1, [0x41, 0x42, 0x43]), obj(2, 4, pattern(40, 8, [1, 2], 3)), eds()]
    out["object-character"] = single([(T0, pes_data(segs))])
    bitmap = bytes((x * 3 + y) % 16 for y in range(8) for x in range(16))
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]),
            rcs(0, 16, 8, depth=4, clut=1, fill=True, bg=(0, 2, 0), objects=[(1, 0, 0)]),
            cds(1, four), ods_progressive(1, 16, 8, zlib.compress(bitmap, 9)), eds()]
    out["object-progressive"] = single([(T0, pes_data(segs))])
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]),
            rcs(0, 16, 8, depth=4, clut=1, objects=[(1, 0, 0, 2, 2, 3)]),
            cds(1, four), obj(1, 4, pattern(16, 8, [1, 2], 3)), eds()]
    out["object-type-composite"] = single([(T0, pes_data(segs))])
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]),
            rcs(0, 16, 8, depth=4, clut=1, objects=[(1, 0, 0, 0, 0, 0, 1)]),
            cds(1, four), obj(1, 4, pattern(16, 8, [1, 2], 3)), eds()]
    out["object-provider-rom"] = single([(T0, pes_data(segs))])
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]),
            rcs(0, 16, 8, depth=4, clut=1, objects=[(1, 0, 0)]),
            cds(1, four), obj(1, 4, pattern(16, 8, [1, 2], 3)),
            segment(DSS, 1, b"\x00\x00\x00\x00\x00"), segment(ACS, 1, b"\x01\x00\x00\x00"),
            segment(0x40, 1, b"\x01\x02"), eds()]
    out["unknown-segments"] = single([(T0, pes_data(segs))])

    # Composition and ancillary pages; other pages ignored.
    anc = [cds(1, four, page=2), obj(1, 4, r0, page=2)]
    comp = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)], page=1)]
    other = [pcs(3, 5, 0, 0, [(0, 300, 300)]), rcs(0, 8, 8, page=3, depth=4, clut=1, fill=True, bg=(0, 5, 0)),
             eds(3)]
    out["pages-ancillary"] = single([(T0, pes_data(anc + comp + other + [eds(1)]))])
    out["pages-other-first"] = single([(T0, pes_data(other + anc + comp + [eds(1)]))])
    # Two services in one descriptor: dvb_substream selects.
    svc = (("eng", 0x10, 1, 2), ("deu", 0x10, 3, 2))
    sets = [(T0, pes_data(anc + comp + [eds(1)])),
            (T0, pes_data([pcs(3, 5, 0, 0, [(0, 100, 100)]),
                           rcs(0, 24, 4, page=3, depth=4, clut=1, objects=[(1, 0, 0)]), eds(3)]))]
    out["services-two"] = single(sets, services=svc)
    # Two subtitle streams in one PMT.
    units = [(0x101, pes(pes_data(anc + comp + [eds(1)]), T0)),
             (0x102, pes(pes_data([pcs(1, 5, 0, 0, [(0, 200, 200)]), rcs(0, 24, 4, depth=2, clut=1, objects=[(1, 0, 0)]),
                                   cds(1, two), obj(1, 2, pattern(24, 4, [0, 1, 2, 3], 3)), eds()]), T0))]
    out["streams-two"] = transport([(0x101, 0x06, subtitling_descriptor([("eng", 0x10, 1, 2)])),
                                    (0x102, 0x06, subtitling_descriptor([("fra", 0x20, 1, 1)]))], units)
    # Ancillary segments after the composition's regions; a second service's
    # page in the same PES; dvb_substream cases select either service.
    out["pages-ancillary-after"] = single([(T0, pes_data(comp + anc + other + [eds(1)]))])
    out["services-two-ordered"] = single([(T0, pes_data(comp + anc + [eds(1)])),
                                          (T0 + 9000, pes_data([pcs(3, 5, 0, 0, [(0, 100, 100)]),
                                                                rcs(0, 24, 4, page=3, depth=4, clut=1, objects=[(1, 0, 0)]),
                                                                obj(1, 4, r1, page=3), eds(3)]))], services=svc)
    out["pages-unsignalled"] = single([(T0, pes_data([pcs(7, 5, 0, 0, [(0, 10, 10)]),
                                                      rcs(0, 24, 4, page=7, depth=4, clut=1, objects=[(1, 0, 0)]),
                                                      cds(1, four, page=9), obj(1, 4, r0, page=8), eds(6)]))])
    # Pages equal: composition page used as ancillary too.
    out["pages-same-id"] = single([(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]),
                                                  rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                                                  cds(1, four), obj(1, 4, r0), eds()]))],
                                  services=(("eng", 0x10, 1, 1),))

    # Timing: PTS wrap, large PTS, sets close together, compute_edt sequences.
    sets = [simple_set(4, r0, four, pts=(1 << 33) - 45000, timeout=4),
            clear_set(45000, 1),
            simple_set(4, r1, four, pts=90000, version=2)]
    out["pts-wrap"] = single(sets)
    sets = []
    pts = T0
    for i in range(12):
        rows = pattern(24, 4, [0, 1 + i % 15], 20 + i)
        sets.append((pts, pes_data([pcs(1, 3 + i % 4, i, 0, [(0, 10 + i, 10)]),
                                    rcs(0, 24, 4, version=i, depth=4, clut=1, objects=[(1, 0, 0)]),
                                    cds(1, four, version=i) if i == 0 else b"",
                                    obj(1, 4, rows, version=i), eds()])))
        pts += [1, 3600, 90000, 450000][i % 4]
        if i % 3 == 2:
            sets.append(clear_set(pts, (i + 100) & 15))
            pts += 900
    out["sequence"] = single(sets)

    # CLUT entries: one table per entry, the shallowest with room; a final
    # reduced entry; a final full entry one byte short.
    regs3 = [pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 30), (2, 10, 50)]),
             rcs(0, 4, 2, depth=2, clut=1, objects=[(1, 0, 0)]), rcs(1, 4, 2, depth=4, clut=1, objects=[(2, 0, 0)]),
             rcs(2, 4, 2, depth=8, clut=1, objects=[(3, 0, 0)])]
    objs3 = [obj(1, 2, [[0, 1, 2, 3]] * 2), obj(2, 4, [[0, 1, 5, 15]] * 2), obj(3, 8, [[0, 1, 5, 17]] * 2), eds()]
    for mask in range(1, 8):
        entries = [full(1, mask, (200, 20, 20)), full(5, mask, (20, 200, 20)), full(17, mask, (20, 20, 200))]
        out["clut-flags-%d" % mask] = single([(T0, pes_data(regs3 + [cds(1, entries)] + objs3))])
    tail = cds(1, [full(1, 7, (90, 90, 90)), reduced(5, 2, 40, 3, 9, 1)])
    out["clut-final-reduced"] = single([(T0, pes_data(regs3 + [tail] + objs3))])
    short = segment(CDS, 1, b"\x01\x00" + bytes([1, 0xe1, 100, 120, 130]) + bytes([5, 0x41, 60, 70, 80]))
    out["clut-final-short"] = single([(T0, pes_data(regs3 + [short] + objs3))])
    # Region resized without fill, partly covered; depth changed in place.
    sets = [simple_set(4, pattern(24, 4, list(range(16)), 41), four, pts=T0),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]),
                                   rcs(0, 30, 6, version=1, depth=4, clut=1, objects=[(1, 2, 1)]),
                                   obj(1, 4, pattern(8, 2, [3, 4], 42), version=1), eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]),
                                    rcs(0, 30, 6, version=2, depth=8, clut=1, objects=[(1, 2, 1)]),
                                    obj(1, 8, pattern(8, 2, [3, 140], 43), version=2), eds()])),
            (T0 + 270000, pes_data([pcs(1, 5, 3, 0, [(0, 10, 10)]),
                                    rcs(0, 30, 6, version=3, depth=2, clut=1), eds()]))]
    out["region-resize-partial"] = single(sets)
    # An 8-bit region recomposed as 2-bit without fill keeps its 8-bit pixel
    # values; the object drawn then has the automatic palette rank them all.
    sets = [(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]),
                           rcs(0, 32, 6, depth=8, clut=1, objects=[(1, 0, 0)]),
                           cds(1, two + eight),
                           obj(1, 8, pattern(32, 6, [0, 5, 3, 140, 200], 46)), eds()])),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]),
                                   rcs(0, 32, 6, version=1, depth=2, clut=1, objects=[(2, 4, 1)]),
                                   obj(2, 2, pattern(12, 2, [1, 2], 47), version=1), eds()]))]
    out["depth-reduced-unfilled"] = single(sets)
    # The automatic palette is computed when a region is presented, from its
    # pixels then, and computed again only after an object is drawn into it:
    # a fill after the last presentation keeps the palette as computed, while
    # a fill after a drawing in the same packet is ranked with it.
    sets = [(T0, pes_data([pcs(1, 5, 0, 0, [(0, 10, 10)]),
                           rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                           obj(1, 4, pattern(24, 4, [1, 2, 3, 4, 5], 61)), eds()])),
            (T0 + 90000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]),
                                   rcs(0, 24, 4, version=1, fill=True, depth=4, clut=1, bg=(0, 7, 0)),
                                   eds()])),
            (T0 + 180000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]),
                                    rcs(0, 24, 4, version=2, depth=4, clut=1, objects=[(1, 4, 1)]),
                                    obj(1, 4, pattern(8, 2, [2, 9], 62), version=1),
                                    rcs(0, 24, 4, version=3, fill=True, depth=4, clut=1, bg=(0, 11, 0),
                                        objects=[(1, 4, 1)]), eds()]))]
    out["fill-after-draw"] = single(sets)
    # One object placed twice in a region and in two regions; a region
    # listed twice on the page.
    segs = [pcs(1, 5, 0, 0, [(0, 10, 10), (1, 10, 40), (0, 200, 10)]),
            rcs(0, 40, 6, depth=4, clut=1, objects=[(1, 0, 0), (1, 20, 2)]),
            rcs(1, 20, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
            cds(1, four), obj(1, 4, pattern(18, 4, list(range(16)), 44)), eds()]
    out["object-placed-twice"] = single([(T0, pes_data(segs))])
    # Unknown sub-block types inside a field, map tables per field, a short
    # map table at the end of a field, non-modifying codes through a map.
    top = b"\x07\x33" + map_table(0x20, [1, 2, 3, 4]) + string(2, [0, 1, 2, 3] * 3) + b"\x99\xf0" + \
        string(2, [3, 2, 1, 0] * 3) + b"\xf0\x21\x05"
    bottom = string(2, [1, 1, 2, 2] * 3) + b"\xf0" + string(2, [0, 0, 3, 3] * 3) + b"\xf0"
    for nm in (False, True):
        segs = [pcs(1, 5, 0, 0, [(0, 10, 10)]), rcs(0, 12, 4, depth=4, clut=1, fill=True, bg=(0, 6, 0), objects=[(1, 0, 0)]),
                cds(1, four), ods(1, top, bottom, nonmodifying=nm), eds()]
        out["field-sub-blocks-%d" % nm] = single([(T0, pes_data(segs))])
    # Display definitions: window then none, short window, repeated version.
    rows = pattern(16, 4, list(range(16)), 45)
    sets = [simple_set(4, rows, four, pts=T0, dds_size=(1280, 720), window=(100, 1179, 60, 659)),
            (T0 + 90000, pes_data([dds(1, 1, 1280, 720), pcs(1, 5, 1, 0, [(0, 40, 50)]), eds()])),
            (T0 + 180000, pes_data([dds(1, 1, 1280, 720, (5, 1274, 7, 699)), pcs(1, 5, 2, 0, [(0, 40, 50)]), eds()])),
            (T0 + 270000, pes_data([segment(DDS, 1, b"\x28\x02\xcf\x02\x3f\x00\x10"), pcs(1, 5, 3, 0, [(0, 40, 50)]), eds()])),
            (T0 + 360000, pes_data([pcs(1, 5, 4, 0, [(0, 40, 50)]), eds()]))]
    out["dds-sequence"] = single(sets)
    # Packets the receiver refuses: junk first, a single short segment.
    sets = [(T0, b"\x20\x00\x0e" + b"".join(a)), (T0 + 1000, pes_data(a)), (T0 + 2000, b"\x20\x00\x0f\x80\x00\x01\x00\x00\xff"),
            (T0 + 3000, pes_data([pcs(1, 5, 7, 0, [(0, 10, 10)]), eds()]))]
    out["packets-refused"] = single(sets)
    # compute_edt sequences: clears, epoch resets, repeated versions.
    sets = [simple_set(4, r0, four, pts=T0),
            (T0 + 9000, pes_data([pcs(1, 5, 0, 0, [(0, 90, 90)]), eds()])),
            (T0 + 18000, pes_data([pcs(1, 7, 1, 1, [(0, 10, 10)]), eds()])),
            (T0 + 18000, pes_data([pcs(1, 7, 2, 0, [(0, 10, 10)]), rcs(0, 24, 4, depth=4, clut=1, objects=[(1, 0, 0)]),
                                   obj(1, 4, r1), eds()])),
            (None, pes_data([pcs(1, 7, 3, 0, [(0, 10, 10)]), eds()])),
            (T0 + 90000, pes_data([pcs(1, 7, 4, 2, []), eds()]))]
    out["edt-sequence"] = single(sets)

    # Repeated object and region versions, a region's CLUT changed, regions
    # without area.
    sets = [simple_set(4, r0, four, pts=T0),
            (T0 + 9000, pes_data([pcs(1, 5, 1, 0, [(0, 10, 10)]), obj(1, 4, r1), eds()])),
            (T0 + 18000, pes_data([pcs(1, 5, 2, 0, [(0, 10, 10)]),
                                   rcs(0, 24, 4, depth=4, clut=2, fill=True, bg=(0, 9, 0), objects=[(1, 4, 1)]),
                                   cds(2, ramp(2, 16, base=33)), eds()])),
            (T0 + 27000, pes_data([pcs(1, 5, 3, 0, [(0, 10, 10)]),
                                   rcs(0, 24, 4, depth=4, clut=2, objects=[(1, 4, 1)]), obj(1, 4, r0), eds()])),
            (T0 + 36000, pes_data([pcs(1, 5, 4, 0, [(0, 10, 10), (5, 50, 50), (6, 60, 60)]),
                                   rcs(5, 0, 6, depth=4, clut=2, fill=True, bg=(0, 3, 0)),
                                   rcs(6, 7, 0, depth=4, clut=2, fill=True, bg=(0, 3, 0), objects=[(1, 0, 0)]),
                                   eds()]))]
    out["versions-repeated"] = single(sets)
    # Several large regions at each depth.
    segs = [pcs(1, 10, 0, 0, [(0, 0, 0), (1, 0, 200), (2, 0, 400)]),
            rcs(0, 720, 160, depth=2, clut=1, objects=[(1, 0, 0)]),
            rcs(1, 720, 160, depth=4, clut=1, objects=[(2, 0, 0)]),
            rcs(2, 720, 160, depth=8, clut=1, objects=[(3, 0, 0)]),
            cds(1, two + four + eight),
            obj(1, 2, pattern(720, 160, [0, 1, 2, 3], 51)),
            obj(2, 4, text_like(720, 160, 5, 9, 0, 52)),
            obj(3, 8, pattern(720, 160, list(range(0, 256, 3)), 53)), eds()]
    out["large-regions"] = single([(T0, pes_data(segs))])

    # Large regions for timing.
    rows = pattern(720, 120, list(range(0, 256, 7)), 31)
    segs = [pcs(1, 10, 0, 0, [(0, 0, 440)]), rcs(0, 720, 120, depth=8, clut=1, objects=[(1, 0, 0)]),
            cds(1, eight), obj(1, 8, rows), eds()]
    out["large-depth8"] = single([(T0, pes_data(segs))])
    sets = []
    for i in range(60):
        tl = text_like(640, 60, fg=1 + i % 3, outline=4, bg=0, seed=i + 1)
        sets.append((T0 + i * 180000, pes_data([pcs(1, 4, i, 0, [(0, 40, 480), (1, 40, 420)]),
                                                rcs(0, 640, 60, version=i, depth=4, clut=1, objects=[(1, 0, 0)]),
                                                rcs(1, 320, 40, version=i, depth=2, clut=2, fill=True, bg=(0, 0, 0),
                                                    objects=[(2, 8, 4)]),
                                                cds(1, four, version=i) if i % 10 == 0 else b"",
                                                cds(2, two, version=i) if i % 10 == 0 else b"",
                                                obj(1, 4, tl, version=i),
                                                obj(2, 2, text_like(300, 30, 1, 2, 0, i), version=i), eds()])))
        sets.append(clear_set(T0 + i * 180000 + 150000, (i + 8) & 15))
    out["long-text"] = single(sets)
    sets = []
    for i in range(40):
        tl = text_like(600, 50, fg=1, outline=2, bg=0, seed=i + 5)
        sets.append((T0 + i * 90000, pes_data([pcs(1, 4, i, 0, [(0, 60, 500)]),
                                               rcs(0, 600, 50, version=i, depth=4, clut=9, objects=[(1, 0, 0)]),
                                               obj(1, 4, tl, version=i), eds()])))
    out["long-default-clut"] = single(sets)
    out.update(automatic_cases())
    out.update(conversion_cases())
    return out


def page_of(regions, depth, clut=9, x0=20, y0=20, version=0, cds_segments=()):
    """One display set: each grid of `regions` in a region of its own, one
    object each, laid out left to right in rows."""
    placements, segs, objs = [], [], []
    x, y, row_h = x0, y0, 0
    for i, rows in enumerate(regions):
        h, w = len(rows), len(rows[0])
        if x + w > 700:
            x, y, row_h = x0, y + row_h + 4, 0
        placements.append((i, x, y))
        segs.append(rcs(i, w, h, version=version, depth=depth, clut=clut, objects=[(i + 1, 0, 0)]))
        objs.append(obj(i + 1, depth, rows, version=version))
        x, row_h = x + w + 4, max(row_h, h)
    return pes_data([pcs(1, 5, version, 0, placements)] + segs + list(cds_segments) + objs + [eds()])


def sea(w, h, cells, value=1):
    rows = [[value] * w for _ in range(h)]
    for y, x, v in cells:
        rows[y][x] = v
    return rows


def run_rows(a_runs, b_runs, width):
    """Runs of 2 then runs of 3, one per row, in a sea of 1."""
    rows = [[1] * width]
    for value, runs in ((2, a_runs), (3, b_runs)):
        for n in runs:
            rows.append([1, 1] + [value] * n + [1] * (width - 2 - n))
            rows.append([1] * width)
    return rows


def split_runs(count, total):
    return [total // count + (1 if i < total % count else 0) for i in range(count)]


def automatic_cases():
    """Automatic palettes (the regions' CLUT is never defined): ranking from
    the region's edge, ties, unused values, every depth, kept entries."""
    out = {}
    block = [(y, x, 2) for y in (3, 4) for x in range(2, 24)] + [(1, x, 2) for x in range(2, 9)]
    block += [(y, x, 3) for y in (8, 9) for x in range(2, 21)] + [(6, x, 3) for x in range(2, 7)]
    ranks = [
        [[1, 2, 1]],                                   # a value at both ends ranks first
        [[0, 2, 4, 1, 3]],                             # distinct values peel from the ends
        [[0, 1, 2], [3, 4, 5], [6, 7, 8]],
        [[8, 1, 5], [0, 7, 4], [3, 6, 2]],
        [[1], [2], [2], [3]],
        [[1, 1], [1, 2]],
        [[2] * 6 + [1] * 4 + [4] * 10 + [3] * 8],      # runs by exposed sides
        sea(9, 7, [(1, 2, 2), (2, 1, 2), (2, 2, 2), (2, 3, 2), (3, 2, 2), (5, 5, 3), (5, 6, 3)]),
        sea(8, 5, [(0, 3, 2), (2, 3, 3), (2, 4, 3)]),  # a value only on the edge
        sea(26, 11, block),                            # equal in 1/1024 steps
        [[5] * 6] * 3,                                 # one value
        [[0, 0, 15, 15], [0, 9, 9, 15]],               # unused values between
    ]
    out["automatic-ranks"] = single([(T0, page_of(ranks, 4))])
    # 1/1024 steps at scale: 64 exposed sides per 32.3 pixels against 32.2, equal
    # in steps; 2.0399 against 2.0404, apart.
    steps = [run_rows([578], split_runs(5, 2648), 582), run_rows([521], split_runs(5, 2324), 525)]
    out["automatic-steps"] = single([(T0, page_of(steps, 4, x0=0, y0=0))])
    two = [pattern(40, 9, [0, 1, 2, 3], seed=71), text_like(48, 12, fg=3, outline=1, bg=2, seed=5),
           pattern(30, 6, [0, 3], seed=72), sea(12, 6, [(2, 4, 0), (2, 5, 0), (3, 4, 3)], value=2), [[3, 0, 3]]]
    out["automatic-depth2"] = single([(T0, page_of(two, 2))])
    eight = [[[(i * 97 + 13) % 256 for i in range(r * 16, r * 16 + 16)] for r in range(16)],
             [list(range(r * 16, r * 16 + 16)) for r in range(16)],
             text_like(60, 14, fg=90, outline=17, bg=200, seed=9),
             pattern(64, 10, list(range(0, 256, 6)), seed=73),
             pattern(20, 20, [250, 3, 77], seed=74)]
    out["automatic-depth8"] = single([(T0, page_of(eight, 8))])
    # Entries of values a later drawing does not use keep their levels; a
    # fill without drawing keeps the palette; a partial drawing recomputes it.
    first = [[1, 1, 2, 2, 3, 3, 4, 4], [1, 2, 3, 4, 5, 6, 7, 8], [8, 8, 8, 1, 1, 1, 2, 2], [5, 5, 6, 6, 7, 7, 8, 8]]
    fewer = [[1] * 8, [1, 2] * 4, [1] * 8, [2] * 8]
    sets = []
    for n, (rows, fill) in enumerate(((first, False), (fewer, False), (None, True), ([[9, 9], [9, 10]], False))):
        segs = [pcs(1, 5, n, 0, [(0, 40, 40)]),
                rcs(0, 8, 4, version=n, depth=4, clut=9, fill=fill, bg=(0, 3, 0), objects=[(1, 0, 0)])]
        if rows:
            segs.append(obj(1, 4, rows, version=n))
        sets.append((T0 + n * 90000, pes_data(segs + [eds()])))
    out["automatic-kept-entries"] = single(sets)
    # compute_clut -2 keeps a region's first automatic palette through redraws,
    # a resize and a depth change; a new epoch (acquisition point) starts afresh.
    third = [[9, 9, 3, 3, 3, 3, 9, 9], [9, 3, 3, 9, 9, 3, 3, 9], [3] * 8, [9] * 8]
    steps = ((0, 8, 4, 4, first), (0, 8, 4, 4, fewer), (0, 4, 2, 4, [[1, 1, 2, 2], [1, 2, 1, 2]]),
             (0, 8, 4, 2, [[1, 2, 3, 0] * 2, [3, 2, 1, 1] * 2]), (1, 8, 4, 4, fewer), (0, 8, 4, 4, third))
    sets = []
    for n, (state, w, h, depth, rows) in enumerate(steps):
        segs = [pcs(1, 5, n, state, [(0, 40, 40)]), rcs(0, w, h, version=n, depth=depth, clut=9, objects=[(1, 0, 0)]),
                obj(1, depth, rows, version=n), eds()]
        sets.append((T0 + n * 90000, pes_data(segs)))
    out["automatic-once"] = single(sets)
    # A checkerboard of 1280 × 1024 pixels: exposure counts past 2^31 / 1024.
    tile = [[0 if (x + y) % 2 == 0 else (2 if y % 4 == 0 and 4 <= y < 60 and x % 3 == 1 and 4 <= x < 60 else 1)
             for x in range(64)] for y in range(64)]
    tiles = [(1, x, y) for y in range(0, 1024, 64) for x in range(0, 1280, 64)]
    segs = [pcs(1, 5, 0, 0, [(0, 0, 0)]), rcs(0, 1280, 1024, depth=4, clut=9, objects=tiles), obj(1, 4, tile), eds()]
    out["automatic-large-exposure"] = single([(T0, pes_data(segs))])
    return out


def conversion_cases():
    """CLUT entries across the conversion's extremes, and entry flags that
    name several CLUTs."""
    out = {}
    grid = [(y, cr, cb, t) for y in (0, 1, 16, 17, 128, 235, 236, 255) for cr in (0, 16, 128, 240, 255)
            for cb in (0, 16, 128, 240, 255) for t in (0, 255)]
    grid += [(y, 128, 128, 0) for y in range(512 - len(grid))]
    cluts = [cds(c + 1, [(i, 1) + grid[c * 256 + i] + (True,) for i in range(256)]) for c in range(2)]
    rows = [list(range(r * 16, r * 16 + 16)) for r in range(16)]
    segs = [pcs(1, 5, 0, 0, [(0, 40, 40), (1, 80, 40)]),
            rcs(0, 16, 16, depth=8, clut=1, objects=[(1, 0, 0)]), rcs(1, 16, 16, depth=8, clut=2, objects=[(1, 0, 0)])]
    out["clut-conversion-grid"] = single([(T0, pes_data(segs + cluts + [obj(1, 8, rows), eds()]))])
    # Entry ids 1, 5 and 20 with every flag combination, in one CLUT each,
    # read back at every depth.
    segs, regions, placements = [], [], []
    for flags in range(1, 8):
        segs.append(cds(flags, [(i, flags, 60 + 20 * flags + i, 90, 160, 0, True) for i in (1, 5, 20)]))
        for k, depth in enumerate((2, 4, 8)):
            rid = (flags - 1) * 3 + k
            placements.append((rid, 30 * k, 30 * flags))
            regions.append(rcs(rid, 24, 1, depth=depth, clut=flags, objects=[(k + 1, 0, 0)]))
    objs = [obj(1, 2, [[0, 1, 2, 3] * 6]), obj(2, 4, [list(range(16)) + [5, 1, 5, 1, 5, 1, 5, 1]]),
            obj(3, 8, [[0, 1, 5, 20] * 6])]
    out["clut-flags-ranges"] = single([(T0, pes_data([pcs(1, 5, 0, 0, placements)] + regions + segs + objs + [eds()]))])
    return out


# ------------------------------------------------------------------ recorded


def recorded(root):
    """The explicit-CLUT case files as one TS each: (name, bytes)."""
    folder = os.path.join(root, "crates", "subtitles", "tests", "explicit-clut-fixtures")
    out = {}
    for name in sorted(os.listdir(folder)):
        if not name.endswith(".case"):
            continue
        with open(os.path.join(folder, name), "rb") as f:
            data = f.read()
        assert data[:8] == b"SMCE0001"
        bits = data[8]
        count = struct.unpack(">H", data[9:11])[0]
        n = 1 << bits
        at = 11
        sets = []
        for _ in range(count):
            _seq, pts, _dts, _dur, _pos, length = struct.unpack(">QqqqqI", data[at:at + 44])
            at += 44
            payload = data[at:at + length]
            at += length + n * 5 + n * 4 * 2 + n * 2 + n * 4
            sets.append((pts, b"\x20\x00" + payload + b"\xff"))
        out["recorded-" + name[:-5]] = single(sets, services=(("eng", 0x10, 1, 1),))
    return out


def recorded_envelopes():
    """The recorded DVB codec packets of the bitmap performance corpus
    (`SMDVB001` envelopes: a 20-byte header, then per packet a flag byte,
    a 64-bit 90 kHz PTS, a 32-bit length and the bytes), one TS each."""
    folder = os.path.join(RESEARCH, "bitmap-subtitle-performance-v1", "corpus")
    out = {}
    if not os.path.isdir(folder):
        return out
    for name in sorted(os.listdir(folder)):
        if not (name.startswith("dvb-original-source--") and name.endswith(".bin")):
            continue
        with open(os.path.join(folder, name), "rb") as f:
            data = f.read()
        assert data[:8] == b"SMDVB001"
        at, sets = 20, []
        while at < len(data):
            pts, length = struct.unpack(">qI", data[at + 1:at + 13])
            payload = data[at + 13:at + 13 + length]
            at += 13 + length
            if not payload.startswith(b"\x20\x00"):
                payload = b"\x20\x00" + payload + b"\xff"
            sets.append((pts, payload))
        out["recorded-" + name[len("dvb-original-source--"):-4]] = single(sets)
    return out


RESEARCH = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), ".research")

# Earlier authored DVB inputs kept in .research, copied into the corpus.
RECORDED_INPUTS = [
    "dvb-ancillary-eod-native-v1-observed/inputs/" + n for n in (
        "ancillary-end-nonempty.mkv", "composition-end-control.mkv", "foreign-end-filter-control.mkv",
        "shared-ancillary-end.mkv", "shared-composition-end-compat.mkv")
] + [
    "dvb-nonmodifying-observed/inputs/dvb%d-%s-flag%d.mkv" % (d, k, f)
    for d in (2, 4, 8) for k in ("literal", "run") for f in (0, 1)
] + [
    "dvb-pes-continuation-v1/segment-cut%d-%s.ts" % (c, k) for c in (4, 25, 99) for k in ("missing", "repeated")
] + [
    "dvb-pes-parser-boundary-v1/authored/no-pts.ts", "dvb-pes-parser-boundary-v1/authored/repeat-pts.ts",
    "mkv-bitmap-dvb-v1/carrier.mkv", "mkv-bitmap-dvb-v1/valid-video-carrier.mkv",
]

# The repository's MPEG-TS DVB test streams.
AUTHORED_INPUTS = ["crates/mpegts/tests/data/dvb/" + n for n in (
    "dvb-ancillary-pages.ts", "dvb-default-clut2.ts", "dvb-nonmodifying.ts", "dvb-partial-alpha-projection.ts",
    "dvb-truncated-segment.ts", "dvb2-literals-fields.ts")]

FATE_INPUTS = ["sub/dvbsubtest_filter.ts"]

# Decoder option variants: (case, [ARG...]).
OPTION_CASES = [
    ("sequence", ["opt:compute_edt=1"]),
    ("page-timeouts-clear", ["opt:compute_edt=1"]),
    ("long-text", ["opt:compute_edt=1"]),
    ("no-end-of-display", ["opt:compute_edt=1"]),
    ("pes-no-pts", ["opt:compute_edt=1"]),
    ("no-pts-between", ["opt:compute_edt=1"]),
    ("no-pts-first", ["opt:compute_edt=1"]),
    ("no-pts-palette-then-fill", ["opt:compute_edt=1"]),
    ("edt-sequence", ["opt:compute_edt=1"]),
    ("edt-sequence", ["opt:compute_edt=1", "opt:compute_clut=1"]),
    ("region-resize-partial", ["opt:compute_clut=1"]),
    ("depth-reduced-unfilled", ["opt:compute_clut=1"]),
    ("fill-after-draw", ["opt:compute_clut=1"]),
    ("pages-ancillary-after", ["opt:dvb_substream=0"]),
    ("pages-other-first", ["opt:dvb_substream=0"]),
    ("pages-unsignalled", ["opt:dvb_substream=0"]),
    ("versions-repeated", ["opt:compute_clut=1"]),
    ("large-regions", ["opt:compute_clut=1"]),
    ("default-clut4-text", ["opt:compute_clut=0"]),
    ("default-clut8", ["opt:compute_clut=0"]),
    ("default-clut2", ["opt:compute_clut=-2"]),
    ("depth4-runs", ["opt:compute_clut=1"]),
    ("depth8-runs", ["opt:compute_clut=1"]),
    ("regions-objects", ["opt:compute_clut=1"]),
    ("page-acquisition", ["opt:compute_clut=-1"]),
    ("services-two-ordered", ["opt:dvb_substream=1"]),
    ("services-two-ordered", ["opt:dvb_substream=0"]),
    ("streams-two", ["stream=1"]),
    ("automatic-ranks", ["opt:compute_clut=1"]),
    ("automatic-kept-entries", ["opt:compute_clut=1"]),
    ("automatic-once", ["opt:compute_clut=-2"]),
]


def reference_fold(reference, path, args):
    out = subprocess.run([reference, "sub-fold", path] + args, capture_output=True, text=True, check=True)
    fold = json.loads(out.stdout.strip().splitlines()[-1])
    return {k: fold[k] for k in ("packets", "bytes", "digest")}


def manifest(corpus, reference, generated):
    """corpus.json with the reference's sub-fold of every case."""
    cases = []

    def add(case_id, rel, rights, source, args):
        path = os.path.join(corpus, rel)
        cases.append({
            "id": case_id, "profile": case_id, "file": rel, "sha256": sha256(path), "rights": rights,
            "source": source, "arguments": ["dvb"] + args,
            "expected": reference_fold(reference, path, ["dvb"] + args),
        })

    for rel in generated:
        name = os.path.basename(rel)
        add(name.replace(".", "-"), rel, "authored-synthetic", "harness/subtitles/dvb_streams.py", [])
    for name, args in OPTION_CASES:
        rel = "dvb/%s.ts" % name
        add("%s--%s" % (name, "-".join(a.split(":")[-1].replace("=", "-") for a in args)), rel,
            "authored-synthetic", "harness/subtitles/dvb_streams.py", args)
    for rel in AUTHORED_INPUTS:
        add("authored-" + os.path.basename(rel).rsplit(".", 1)[0], "dvb/authored/" + os.path.basename(rel),
            "authored-synthetic", rel, [])
    for rel in RECORDED_INPUTS:
        base = rel.split("/")[0] + "--" + os.path.basename(rel)
        add("research-" + base.rsplit(".", 1)[0], "dvb/research/" + base, "authored-synthetic", ".research/" + rel, [])
    for rel in FATE_INPUTS:
        add("fate-" + os.path.basename(rel).rsplit(".", 1)[0], "fate/" + rel, "fetched-not-redistributed",
            "fate-suite/" + rel, [])
    return {"schema": 1, "cases": cases}


def sha256(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


# ------------------------------------------------------------------ output


def mux_mkv(path):
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        raise SystemExit("ffmpeg is required to mux the Matroska copies")
    target = path[:-3] + ".mkv"
    subprocess.run([ffmpeg, "-v", "error", "-y", "-i", path, "-map", "0:s", "-c", "copy",
                    "-output_ts_offset", "1", "-fflags", "+bitexact", target], check=True)
    return target


MKV_COPIES = ("depth4-runs", "regions-objects", "page-timeouts-clear", "dds-window", "default-clut4-text",
              "sequence", "pages-ancillary", "long-text")


FIXTURE_LIMIT = 8000  # codec bytes per case
FIXTURE_FILE = 60000  # bytes per fixture file


def microseconds(value, num, den):
    """`value` ticks of num/den seconds in microseconds, nearest, ties away."""
    n = value * num * 1000000
    q, r = divmod(abs(n), den)
    if 2 * r >= den:
        q += 1
    return q if n >= 0 else -q


def fixtures(corpus, reference, cases, folder):
    """Write the codec packets, options and reference fold of every case with
    at most FIXTURE_LIMIT codec bytes into `folder` as DVBR0001 files:
    per case a u16-length id, the options (compute_edt u8, compute_clut i8,
    dvb_substream i8), a u16-length configuration, a u32 packet count, per
    packet a u8 PTS flag, an i64 PTS in microseconds, a u32-length payload,
    then the expected packets, bytes and digest as u64."""
    os.makedirs(folder, exist_ok=True)
    for name in os.listdir(folder):
        if name.endswith(".bin"):
            os.remove(os.path.join(folder, name))
    records = []
    for case in cases:
        path = os.path.join(corpus, case["file"])
        lines = subprocess.run([reference, "sub-input", path] + case["arguments"], capture_output=True, text=True,
                               check=True).stdout.splitlines()
        head = json.loads(lines[0])
        num, den = head["time_base"]
        packets = [json.loads(line) for line in lines[1:]]
        if sum(len(p["data"]) // 2 for p in packets) > FIXTURE_LIMIT:
            continue
        options = {"compute_edt": 0, "compute_clut": -1, "dvb_substream": -1}
        for arg in case["arguments"][1:]:
            if arg.startswith("opt:"):
                key, value = arg[4:].split("=")
                options[key] = int(value)
        out = bytearray()
        ident = case["id"].encode()
        config = bytes.fromhex(head["configuration"])
        out += struct.pack(">H", len(ident)) + ident
        out += struct.pack(">Bbb", options["compute_edt"], options["compute_clut"], options["dvb_substream"])
        out += struct.pack(">H", len(config)) + config + struct.pack(">I", len(packets))
        for p in packets:
            data = bytes.fromhex(p["data"])
            pts = p["pts"]
            out += struct.pack(">Bq", pts is not None, microseconds(pts, num, den) if pts is not None else 0)
            out += struct.pack(">I", len(data)) + data
        expected = case["expected"]
        out += struct.pack(">QQQ", expected["packets"], expected["bytes"], expected["digest"])
        records.append(bytes(out))
    files, current = [], bytearray()
    for record in records:
        if current and len(current) + len(record) > FIXTURE_FILE:
            files.append(current)
            current = bytearray()
        current += record
    files.append(current)
    for i, data in enumerate(files):
        with open(os.path.join(folder, "cases-%d.bin" % i), "wb") as f:
            f.write(b"DVBR0001" + bytes(data))
    return len(records)


def main(argv):
    record = "--recorded" in argv
    write_manifest = "--manifest" in argv
    args = [a for a in argv if not a.startswith("--")]
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    corpus = os.path.join(root, ".research", "subtitle-corpus")
    out = args[0] if args else os.path.join(corpus, "dvb")
    os.makedirs(out, exist_ok=True)
    streams = cases()
    if record:
        streams.update(recorded(root))
        streams.update(recorded_envelopes())
    generated = []
    for name, data in sorted(streams.items()):
        path = os.path.join(out, name + ".ts")
        with open(path, "wb") as f:
            f.write(data)
        generated.append("dvb/" + name + ".ts")
        if name in MKV_COPIES:
            mux_mkv(path)
            generated.append("dvb/" + name + ".mkv")
    if record:
        for folder, sources, base in (("authored", AUTHORED_INPUTS, root), ("research", RECORDED_INPUTS, RESEARCH)):
            os.makedirs(os.path.join(out, folder), exist_ok=True)
            for rel in sources:
                name = os.path.basename(rel) if folder == "authored" else rel.split("/")[0] + "--" + os.path.basename(rel)
                shutil.copyfile(os.path.join(base, rel), os.path.join(out, folder, name))
        for rel in FATE_INPUTS:
            target = os.path.join(corpus, "fate", rel)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copyfile(os.path.join(RESEARCH, "fate-samples", rel), target)
    if write_manifest:
        reference = os.path.join(root, "target", "harness", "ffmpeg-subtitles-worker")
        data = manifest(corpus, reference, generated)
        target = os.path.join(root, "harness", "features", "subtitle-dvb", "corpus.json")
        with open(target, "w") as f:
            json.dump(data, f, indent=2)
            f.write("\n")
        print("%d cases -> %s" % (len(data["cases"]), target))
        if "--fixtures" in argv:
            folder = os.path.join(root, "crates", "subtitles", "tests", "dvb-receiver-fixtures")
            print("%d fixture cases -> %s" % (fixtures(corpus, reference, data["cases"], folder), folder))


if __name__ == "__main__":
    main(sys.argv[1:])
