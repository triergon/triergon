#!/usr/bin/env python3
"""Deterministic PGS (HDMV presentation graphics) streams for the subtitle corpus.

Builds presentation graphics segments (PCS 0x16, WDS 0x17, PDS 0x14, ODS 0x15,
END 0x80) with run-length coded objects, and carries them three ways:

* `.sup`: one `PG` unit (magic, 32-bit PTS and DTS on a 90 kHz clock, the
  segment) per segment;
* `.mkv`: one Matroska `S_HDMV/PGS` SimpleBlock per packet (a whole display set,
  or whatever the case puts in it), millisecond timestamps;
* `.ts` / `.m2ts`: one PES packet (private_stream_1, PTS and optional DTS) per
  packet on PID 0x1200, stream type 0x90, in a program with an `HDMV`
  registration descriptor; M2TS adds the 4-byte arrival-time prefix.

All writers are local and byte for byte the same on every run. The recorded
packet inputs of `crates/subtitles/tests/native-compatibility-gold/` (authored
transcripts of FFmpeg's receiver) are converted into TS files with one PES per
recorded packet when their clocks fit a 90 kHz PES (see `gold_cases`).

The FATE sample `sub/pgs_sub.sup` is copied into the corpus with Matroska and
M2TS muxes of it (see `fate_cases`). With the reference worker built
(`target/harness/ffmpeg-subtitles-worker`), the manifest
`harness/features/subtitle-pgs/corpus.json` records its `sub-fold` of every
case; inputs it refuses to prepare are listed under `refused`.

usage: pgs_streams.py   (writes .research/subtitle-corpus/pgs and fate/sub)
"""

import hashlib
import json
import os
import struct
import subprocess
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GOLD = os.path.join(ROOT, "crates/subtitles/tests/native-compatibility-gold")

# ------------------------------------------------------------------ segments


def segment(kind, body):
    return bytes([kind]) + struct.pack(">H", len(body)) + body


def pcs(width, height, number, state, objects=(), palette_update=0, palette_id=0, rate=0x10):
    """objects: (id, window, x, y, forced, crop) with crop None or (x, y, w, h)."""
    body = struct.pack(">HHBHBBBB", width, height, rate, number, state, palette_update,
                       palette_id, len(objects))
    for oid, window, x, y, forced, crop in objects:
        flags = (0x40 if forced else 0) | (0x80 if crop else 0)
        body += struct.pack(">HBBHH", oid, window, flags, x, y)
        if crop:
            body += struct.pack(">HHHH", *crop)
    return segment(0x16, body)


def wds(windows):
    body = bytes([len(windows)])
    for wid, x, y, w, h in windows:
        body += struct.pack(">BHHHH", wid, x, y, w, h)
    return segment(0x17, body)


def pds(pid, version, entries):
    body = bytes([pid, version])
    for index, y, cr, cb, a in entries:
        body += bytes([index, y, cr, cb, a])
    return segment(0x14, body)


def ods_parts(oid, version, width, height, rle, pieces=1):
    """The ODS segments of one object, its RLE split into `pieces` fragments."""
    step = max(1, -(-len(rle) // pieces))
    chunks = [rle[i:i + step] for i in range(0, len(rle), step)] or [b""]
    out = []
    for n, chunk in enumerate(chunks):
        flags = (0x80 if n == 0 else 0) | (0x40 if n == len(chunks) - 1 else 0)
        body = struct.pack(">HBB", oid, version, flags)
        if n == 0:
            body += (len(rle) + 4).to_bytes(3, "big") + struct.pack(">HH", width, height)
        out.append(segment(0x15, body + chunk))
    return out


def ods_raw(oid, version, flags, data, length=None, width=None, height=None):
    body = struct.pack(">HBB", oid, version, flags)
    if flags & 0x80:
        body += length.to_bytes(3, "big") + struct.pack(">HH", width, height)
    return segment(0x15, body + data)


END = segment(0x80, b"")


def rle(rows):
    """PGS run-length code for rows of colour indices, each row ended by 00 00."""
    out = bytearray()
    for row in rows:
        i = 0
        while i < len(row):
            c = row[i]
            n = 1
            while i + n < len(row) and row[i + n] == c and n < 16383:
                n += 1
            if c != 0 and n < 3:
                out += bytes([c]) * n
            elif c == 0:
                out += bytes([0, n]) if n < 64 else bytes([0, 0x40 | n >> 8, n & 255])
            else:
                out += bytes([0, 0x80 | n, c]) if n < 64 else bytes([0, 0xC0 | n >> 8, n & 255, c])
            i += n
        out += b"\0\0"
    return bytes(out)


def pattern(width, height, seed, colours=255):
    """Deterministic rows mixing short and long runs of colours 0..colours."""
    rows = []
    state = seed * 2654435761 & 0xFFFFFFFF
    for y in range(height):
        row = []
        while len(row) < width:
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            run = 1 + (state >> 8) % (1 if state & 3 == 0 else 40)
            colour = 0 if state & 0x30 == 0 else 1 + (state >> 16) % colours
            row += [colour] * min(run, width - len(row))
        rows.append(row)
    return rows


def palette(n=256, shift=0):
    return [(i, (16 + 7 * i + shift) & 255, (128 + 3 * i) & 255, (128 - 5 * i) & 255,
             0 if i == 0 else (64 + 11 * i) & 255) for i in range(n)]


def display(width, height, number, state, objects, windows, pal, pal_version=0,
            pal_id=0, end=True, pieces=1, palette_update=0):
    """One display set: PCS, WDS, PDS, the ODS of every object, END.

    objects: (id, window, x, y, forced, crop, w, h, seed) or with a raw RLE as
    the ninth field (bytes)."""
    refs = [o[:6] for o in objects]
    out = [pcs(width, height, number, state, refs, palette_update, pal_id)]
    if windows is not None:
        out.append(wds(windows))
    if pal is not None:
        out.append(pds(pal_id, pal_version, pal))
    seen = set()
    for o in objects:
        oid, w, h, art = o[0], o[6], o[7], o[8]
        if oid in seen or w == 0:
            continue
        seen.add(oid)
        data = art if isinstance(art, bytes) else rle(pattern(w, h, art))
        out += ods_parts(oid, 0, w, h, data, pieces)
    if end:
        out.append(END)
    return out

# ------------------------------------------------------------------- writers


def write_sup(units):
    """units: (pts, dts, [segment...]); one PG header per segment."""
    out = bytearray()
    for pts, dts, segments in units:
        for seg in segments:
            out += b"PG" + struct.pack(">II", pts & 0xFFFFFFFF, dts & 0xFFFFFFFF) + seg
    return bytes(out)


def ebml_id(value):
    return value.to_bytes((value.bit_length() + 7) // 8, "big")


def ebml_size(n):
    for length in range(1, 9):
        if n < (1 << (7 * length)) - 1:
            return ((1 << (7 * length)) | n).to_bytes(length, "big")
    raise ValueError(n)


def element(eid, payload):
    return ebml_id(eid) + ebml_size(len(payload)) + payload


def uint(eid, value):
    return element(eid, value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big"))


def write_mkv(units):
    """One cluster per packet, a SimpleBlock on track 1; pts in 90 kHz ticks,
    written as milliseconds (cases use whole milliseconds)."""
    header = element(0x1A45DFA3, uint(0x4286, 1) + uint(0x42F7, 1) + uint(0x42F2, 4)
                     + uint(0x42F3, 8) + element(0x4282, b"matroska") + uint(0x4287, 4)
                     + uint(0x4285, 2))
    info = element(0x1549A966, uint(0x2AD7B1, 1000000) + element(0x4D80, b"pgs_streams")
                   + element(0x5741, b"pgs_streams"))
    track = element(0xAE, uint(0xD7, 1) + uint(0x73C5, 1) + uint(0x83, 0x11) + uint(0x9C, 0)
                    + element(0x86, b"S_HDMV/PGS"))
    body = info + element(0x1654AE6B, track)
    for pts, _dts, segments in units:
        ms = pts // 90
        block = b"\x81" + struct.pack(">hB", 0, 0x80) + b"".join(segments)
        body += element(0x1F43B675, uint(0xE7, ms) + element(0xA3, block))
    return header + element(0x18538067, body)


def crc32_mpeg(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = (crc << 1) ^ 0x04C11DB7 if crc & 0x80000000 else crc << 1
            crc &= 0xFFFFFFFF
    return crc


def section(table_id, extension, payload):
    body = struct.pack(">HBBB", extension, 0xC1, 0, 0) + payload
    head = bytes([table_id]) + struct.pack(">H", 0xB000 | (len(body) + 4))
    data = head + body
    return data + struct.pack(">I", crc32_mpeg(data))


class Transport:
    def __init__(self, m2ts, streams=1):
        self.m2ts = m2ts
        self.streams = streams
        self.out = bytearray()
        self.cc = {}
        self.ats = 0

    def packet(self, pid, payload, start, stuff_to=184):
        cc = self.cc.get(pid, 0)
        self.cc[pid] = (cc + 1) & 15
        head = struct.pack(">BHB", 0x47, (0x4000 if start else 0) | pid, 0x10 | cc)
        if len(payload) < 184:
            pad = 184 - len(payload)
            if pad == 1:
                adaptation = b"\x00"
            else:
                adaptation = bytes([pad - 1, 0]) + b"\xff" * (pad - 2)
            head = head[:3] + bytes([0x30 | cc])
            payload = adaptation + payload
        if self.m2ts:
            self.out += struct.pack(">I", self.ats & 0x3FFFFFFF)
            self.ats += 1000
        self.out += head + payload

    def psi(self, pid, data):
        self.packet(pid, (b"\x00" + data).ljust(184, b"\xff"), True)

    def tables(self):
        self.psi(0, section(0x00, 1, struct.pack(">HH", 1, 0xE000 | 0x100)))
        info = b"\x05\x04HDMV"
        pmt = struct.pack(">HH", 0xE000 | 0x1FFF, 0xF000 | len(info)) + info
        for n in range(self.streams):
            pmt += struct.pack(">BHH", 0x90, 0xE000 | (0x1200 + n), 0xF000)
        self.psi(0x100, section(0x02, 1, pmt))

    def pes(self, pid, pts, dts, data):
        def stamp(prefix, value):
            return bytes([prefix << 4 | (value >> 29 & 0x0E) | 1, value >> 22 & 255,
                          (value >> 14 & 0xFE) | 1, value >> 7 & 255, (value << 1 & 0xFE) | 1])
        optional = b""
        flags = 0
        if pts is not None and dts is not None and dts != pts:
            optional = stamp(3, pts) + stamp(1, dts)
            flags = 0xC0
        elif pts is not None:
            optional = stamp(2, pts)
            flags = 0x80
        header = bytes([0x84, flags, len(optional)]) + optional
        length = len(header) + len(data)
        pes = b"\x00\x00\x01\xBD" + struct.pack(">H", length if length < 65536 else 0) + header + data
        for i in range(0, len(pes), 184):
            self.packet(pid, pes[i:i + 184], i == 0)


def write_ts(units, m2ts=False, second=None):
    """One PES per packet on PID 0x1200; pts/dts None leave the field out.
    `second` puts another stream's units on PID 0x1201, interleaved."""
    ts = Transport(m2ts, 2 if second else 1)
    streams = [(0x1200, units)] + ([(0x1201, second)] if second else [])
    for n in range(max(len(u) for _, u in streams)):
        if n % 16 == 0:
            ts.tables()
        for pid, stream in streams:
            if n < len(stream):
                pts, dts, segments = stream[n]
                ts.pes(pid, pts, dts, b"".join(segments))
    return bytes(ts.out)

# --------------------------------------------------------------------- cases

HD = (1920, 1080)
SD = (720, 480)


def epoch(t, objects, windows=None, number=0, state=0x80, pal=None, size=HD, **kw):
    """A unit at 90 kHz time t holding one display set."""
    if windows is None:
        windows = [(o[1], o[2], o[3], max(o[6], 1), max(o[7], 1)) for o in objects]
        windows = list({w[0]: w for w in windows}.values())
    return (t, t, display(size[0], size[1], number, state, objects, windows,
                          palette() if pal is None else pal, **kw))


def clear(t, number=1, size=HD, end=True):
    segs = [pcs(size[0], size[1], number, 0x00), wds([])]
    if end:
        segs.append(END)
    return (t, t, segs)


def synthetic_cases():
    """(name, units, carriers, options): carriers among sup/mkv/ts/m2ts."""
    T = 90000
    cases = []

    def add(name, units, carriers=("sup",), options=()):
        cases.append((name, units, carriers, list(options)))

    one = (1, 0, 600, 900, False, None, 400, 60, 1)
    add("basic", [epoch(T, [one]), clear(3 * T)], ("sup", "mkv", "ts", "m2ts"))
    two = [(1, 0, 100, 800, False, None, 300, 50, 2), (2, 1, 900, 950, False, None, 500, 70, 3)]
    add("two-objects-two-windows", [epoch(T, two), clear(2 * T)], ("sup", "mkv", "ts"))
    same_window = [(1, 0, 100, 800, False, None, 300, 40, 4), (2, 0, 100, 860, False, None, 300, 40, 5)]
    add("two-objects-one-window", [epoch(T, same_window, windows=[(0, 100, 800, 300, 100)]),
                                   clear(2 * T)])
    three = two + [(3, 1, 500, 100, True, None, 100, 20, 6)]
    add("three-objects", [epoch(T, three), clear(2 * T)])
    add("same-object-twice", [epoch(T, [one, (1, 0, 10, 10, True, None, 400, 60, 1)]), clear(2 * T)])
    cropped = (1, 0, 600, 900, False, (10, 5, 100, 20), 400, 60, 7)
    add("cropped", [epoch(T, [cropped]), clear(2 * T)], ("sup", "mkv"))
    crop_out = (1, 0, 600, 900, False, (390, 50, 100, 100), 400, 60, 7)
    add("crop-outside-object", [epoch(T, [crop_out]), clear(2 * T)])
    forced = [(1, 0, 100, 800, True, None, 300, 50, 8), (2, 1, 900, 950, False, None, 500, 70, 9)]
    for flag in ("0", "1"):
        add(f"forced-mixed-only{flag}", [epoch(T, forced), clear(2 * T)], ("sup", "mkv"),
            [f"opt:forced_subs_only={flag}"])
    add("forced-none-only1", [epoch(T, two), clear(2 * T)], ("sup",), ["opt:forced_subs_only=1"])
    # Palette-only update: same composition, new palette version, update flag set.
    add("palette-only-update", [
        epoch(T, [one]),
        (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x00, [one[:6]], palette_update=0x80),
                        pds(0, 1, palette(256, 40)), END]),
        (3 * T, 3 * T, [pcs(1920, 1080, 2, 0x00, [one[:6]], palette_update=0x80),
                        pds(0, 2, palette(16, 90)), END]),
        clear(4 * T, 3)], ("sup", "mkv", "ts"))
    add("palette-update-flag-without-pds", [
        epoch(T, [one]),
        (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x00, [one[:6]], palette_update=0x80), END]),
        clear(3 * T, 2)])
    # Fragmented objects.
    big = (1, 0, 0, 0, False, None, 1900, 200, 10)
    add("fragmented-3", [epoch(T, [big], pieces=3), clear(2 * T)], ("sup", "mkv", "ts"))
    add("fragmented-40", [epoch(T, [big], pieces=40), clear(2 * T)], ("sup", "ts"))
    data = rle(pattern(300, 40, 11))
    add("fragments-interleaved", [(T, T, [
        pcs(1920, 1080, 0, 0x80, [(1, 0, 0, 0, False, None), (2, 0, 0, 500, False, None)]),
        wds([(0, 0, 0, 1920, 1080)]), pds(0, 0, palette()),
        ods_raw(1, 0, 0x80, data[:100], len(data) + 4, 300, 40),
        ods_raw(2, 0, 0x80, data[:50], len(data) + 4, 300, 40),
        ods_raw(1, 0, 0x40, data[100:]), ods_raw(2, 0, 0x40, data[50:]), END]),
        clear(2 * T)])
    # Composition states across an epoch.
    a = (1, 0, 100, 900, False, None, 200, 40, 12)
    b = (2, 1, 1000, 900, False, None, 200, 40, 13)
    add("epoch-acquisition-normal", [
        epoch(T, [a, b], windows=[(0, 100, 900, 200, 40), (1, 1000, 900, 200, 40)]),
        (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x00, [a[:6]]), wds([(0, 100, 900, 200, 40)]), END]),
        (3 * T, 3 * T, [pcs(1920, 1080, 2, 0x00, [b[:6], a[:6]]), END]),
        epoch(4 * T, [(1, 0, 100, 900, False, None, 200, 40, 14)], number=3, state=0x40),
        (5 * T, 5 * T, [pcs(1920, 1080, 4, 0x00, [b[:6]]), END]),
        epoch(6 * T, [b], number=5, state=0x80),
        (7 * T, 7 * T, [pcs(1920, 1080, 6, 0x00, [a[:6]]), END]),
        clear(8 * T, 7)], ("sup", "mkv", "ts"))
    add("state-c0-and-reserved", [
        epoch(T, [a], state=0xC0), epoch(2 * T, [b], state=0x20, number=1),
        epoch(3 * T, [a], state=0x01, number=2), clear(4 * T, 3)])
    # Malformed and truncated segments.
    good = epoch(T, [one])
    add("pcs-short", [(T, T, [segment(0x16, b"\x07\x80\x04\x38\x10\x00")]), good,
                      (2 * T, 2 * T, [segment(0x16, b""), END])])
    add("pcs-object-count-overrun", [
        (T, T, [segment(0x16, struct.pack(">HHBHBBBB", 1920, 1080, 0x10, 0, 0x80, 0, 0, 3)
                        + struct.pack(">HBBHH", 1, 0, 0, 10, 10))]),
        (T, T, [pds(0, 0, palette()), *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))), END]),
        clear(2 * T)])
    add("pcs-trailing-bytes", [
        (T, T, [segment(0x16, pcs(1920, 1080, 0, 0x80, [one[:6]])[3:] + b"\x01\x02\x03")]),
        (T, T, [pds(0, 0, palette()), *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))), END]),
        clear(2 * T)])
    add("pcs-crop-truncated", [
        (T, T, [segment(0x16, pcs(1920, 1080, 0, 0x80, [cropped[:6]])[3:-4])]),
        (T, T, [pds(0, 0, palette()), *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 7))), END]),
        clear(2 * T)])
    add("pds-partial-entry", [(T, T, display(1920, 1080, 0, 0x80, [one], None, None)[:1]
                               + [segment(0x14, b"\x00\x00" + b"\x01\x10\x80\x80\xff\x02\x20")]
                               + display(1920, 1080, 0, 0x80, [one], None, None)[1:]),
                              clear(2 * T)])
    add("pds-short", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), segment(0x14, b"\x00"),
                              *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))), END]),
                      clear(2 * T)])
    add("pds-entry-beyond-255", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]),
                                         pds(0, 0, palette(256)), pds(0, 1, palette(256, 9)),
                                         *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))), END])])
    add("palette-ids-nine", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]], palette_id=8)]
                              + [pds(i, 0, palette(8, i)) for i in range(9)]
                              + ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))) + [END]),
                             clear(2 * T)])
    add("palette-id-missing", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]], palette_id=3),
                                       pds(0, 0, palette()),
                                       *ods_parts(1, 0, 400, 60, rle(pattern(400, 60, 1))), END]),
                               clear(2 * T)])
    add("ods-short-first", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                    ods_raw(1, 0, 0xC0, b"", 4, 0, 0)[:-2], END]), good, clear(2 * T)])
    add("ods-zero-size", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                  ods_raw(1, 0, 0xC0, b"\0\0", 6, 0, 60), END]), clear(2 * T)])
    add("ods-larger-than-canvas", [(T, T, [pcs(720, 480, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                           *ods_parts(1, 0, 800, 10, rle(pattern(800, 10, 1))), END]),
                                   clear(2 * T)])
    add("ods-length-short", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                     ods_raw(1, 0, 0xC0, rle(pattern(400, 60, 1)), 10, 400, 60), END]),
                             clear(2 * T)])
    data = rle(pattern(400, 60, 1))
    add("ods-length-long", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                    ods_raw(1, 0, 0xC0, data, len(data) + 200, 400, 60), END]),
                            clear(2 * T)])
    add("ods-continuation-without-first", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]),
                                                   pds(0, 0, palette()),
                                                   ods_raw(1, 0, 0x40, data[:200]), END]),
                                           clear(2 * T)])
    add("ods-missing-last", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                     ods_raw(1, 0, 0x80, data, len(data) + 4, 400, 60), END]),
                             clear(2 * T)])
    add("ods-flags-zero", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), pds(0, 0, palette()),
                                   ods_raw(1, 0, 0x80, data[:300], len(data) + 4, 400, 60),
                                   ods_raw(1, 0, 0x00, data[300:]), END]), clear(2 * T)])
    rows = pattern(400, 60, 1)
    bad = bytearray(rle(rows))
    add("rle-short-rows", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, rle([r[:200] for r in rows]))]),
                           clear(2 * T)])
    add("rle-long-rows", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, rle([r + r for r in rows]))]),
                          clear(2 * T)])
    add("rle-too-few-rows", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, rle(rows[:30]))]),
                             clear(2 * T)])
    add("rle-too-many-rows", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, rle(rows + rows))]),
                              clear(2 * T)])
    add("rle-truncated", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, bytes(bad[:len(bad) // 2]))]),
                          clear(2 * T)])
    add("rle-no-final-eol", [epoch(T, [(1, 0, 600, 900, False, None, 400, 60, bytes(bad[:-2]))]),
                             clear(2 * T)])
    add("rle-zero-length-runs", [epoch(T, [(1, 0, 600, 900, False, None, 4, 2,
                                            b"\x00\x00\x01\x00\x80\x02\x00\x40\x00\x00\x00\x00\xc0\x00\x05\x03\x00\x00")]),
                                 clear(2 * T)])
    add("unknown-segment", [(T, T, display(1920, 1080, 0, 0x80, [one], None, palette())[:2]
                             + [segment(0x18, b"\x01\x02\x03"), segment(0x00, b"")]
                             + display(1920, 1080, 0, 0x80, [one], None, palette())[2:]),
                            clear(2 * T)])
    add("end-with-payload", [(T, T, display(1920, 1080, 0, 0x80, [one], None, palette())[:-1]
                              + [segment(0x80, b"\x01\x02")]), clear(2 * T)])
    add("wds-truncated", [(T, T, [pcs(1920, 1080, 0, 0x80, [one[:6]]), segment(0x17, b"\x02\x00\x00"),
                                  pds(0, 0, palette()), *ods_parts(1, 0, 400, 60, data), END]),
                          clear(2 * T)])
    add("missing-end", [
        (T, T, display(1920, 1080, 0, 0x80, [one], [(0, 600, 900, 400, 60)], palette(), end=False)),
        (2 * T, 2 * T, display(1920, 1080, 1, 0x80, [(1, 0, 600, 900, False, None, 400, 60, 15)],
                               [(0, 600, 900, 400, 60)], palette(), end=False)),
        clear(3 * T, 2, end=False), epoch(4 * T, [one], number=3), clear(5 * T, 4)], ("sup", "mkv", "ts"))
    add("end-without-pcs", [(T, T, [END]), (T, T, [pds(0, 0, palette()), END]), epoch(2 * T, [one]),
                            clear(3 * T)])
    add("end-twice", [(T, T, display(1920, 1080, 0, 0x80, [one], None, palette()) + [END]),
                      clear(2 * T), (2 * T, 2 * T, [END])])
    add("out-of-order", [
        (T, T, [pds(0, 0, palette()), *ods_parts(1, 0, 400, 60, data),
                pcs(1920, 1080, 0, 0x80, [one[:6]]), END]),
        (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x80, [one[:6]]), END, pds(0, 0, palette()),
                        *ods_parts(1, 0, 400, 60, data)]),
        (3 * T, 3 * T, [pcs(1920, 1080, 2, 0x00, [one[:6]]), END]),
        (4 * T, 4 * T, [wds([(0, 0, 0, 10, 10)]), END, pcs(1920, 1080, 3, 0x00, [])]),
        clear(5 * T, 4)], ("sup", "mkv", "ts"))
    add("object-cache-across-epochs", [
        epoch(T, [one]),
        (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x80, [one[:6]]), pds(0, 0, palette()), END]),
        (3 * T, 3 * T, [pcs(1920, 1080, 2, 0x00, [one[:6]]), END]), clear(4 * T, 3)])
    add("position-outside-canvas", [epoch(T, [(1, 0, 1900, 1070, False, None, 400, 60, 16)]),
                                    epoch(2 * T, [(1, 0, 1921, 0, False, None, 40, 6, 16)], number=1),
                                    epoch(3 * T, [(1, 0, 0, 1081, False, None, 40, 6, 16)], number=2),
                                    clear(4 * T, 3)])
    add("canvas-sizes", [epoch(T, [(1, 0, 10, 10, False, None, 100, 20, 17)], size=(720, 576)),
                         epoch(2 * T, [(1, 0, 10, 10, False, None, 100, 20, 17)], size=(720, 480), number=1),
                         epoch(3 * T, [(1, 0, 10, 10, False, None, 100, 20, 17)], size=(1280, 720), number=2),
                         epoch(4 * T, [(1, 0, 10, 10, False, None, 100, 20, 17)], size=(0, 0), number=3),
                         clear(5 * T, 4)])
    objs = [(i, 0, 10 * i, 10, False, None, 4, 2, 20 + i) for i in range(1, 70)]
    add("many-objects", [(T, T, [pcs(1920, 1080, 0, 0x80, [o[:6] for o in objs[:2]]),
                                 wds([(0, 0, 0, 1920, 1080)]), pds(0, 0, palette())]
                          + [s for o in objs for s in ods_parts(o[0], 0, 4, 2, rle(pattern(4, 2, o[8])))]
                          + [END]),
                         (2 * T, 2 * T, [pcs(1920, 1080, 1, 0x00, [objs[66][:6], objs[3][:6]]), END]),
                         clear(3 * T, 2)])
    add("pts-zero-and-wrap", [epoch(0, [one]), clear(90), epoch(0xFFFFFF00, [one], number=2),
                              clear(0x10, 3)])
    add("dts-variants", [(T, 0, display(1920, 1080, 0, 0x80, [one], None, palette())),
                         (2 * T, 2 * T + 5, [pcs(1920, 1080, 1, 0, []), END]),
                         (3 * T, 3 * T - 900, [pcs(1920, 1080, 2, 0x80, [one[:6]]), pds(0, 0, palette()),
                                               *ods_parts(1, 0, 400, 60, data), END]),
                         clear(4 * T, 3)])
    # SUP framing faults. The reference skips one unit without its magic while
    # it still probes the stream (before a PCS with a valid canvas), and fails
    # on any other.
    sup_good = write_sup([epoch(T, [one]), clear(2 * T)])
    lead = write_sup([(T, T, [pds(0, 0, palette())])])
    cases.append(("sup-junk-while-probing", None, ("raw",), [],
                  lead + b"XX" + write_sup([epoch(T, [one]), clear(2 * T)])))
    cases.append(("sup-junk-after-probing", None, ("raw",), [],
                  write_sup([epoch(T, [one])]) + b"XX" + write_sup([clear(2 * T)])))
    cases.append(("sup-junk-twice", None, ("raw",), [],
                  b"XX" + lead + b"XX" + write_sup([epoch(T, [one]), clear(2 * T)])))
    cases.append(("sup-truncated-tail", None, ("raw",), [], sup_good[:-200]))
    cases.append(("sup-truncated-header", None, ("raw",), [], sup_good + b"PG\x00\x00\x00\x01\x00\x00\x00\x00\x80"))
    cases.append(("sup-partial-magic-tail", None, ("raw",), [], sup_good + b"PGX"))
    cases.append(("sup-empty-body-tail", None, ("raw",), [],
                  sup_good + b"PG\x00\x00\x00\x01\x00\x00\x00\x00\x16\x00\x05"))
    cases.append(("sup-one-body-byte-tail", None, ("raw",), [],
                  sup_good + b"PG\x00\x00\x00\x01\x00\x00\x00\x00\x16\x00\x05\x07"))
    cases += receiver_cases()
    # Long stream for timing: many display sets of mixed sizes.
    long_units = []
    for n in range(240):
        t = T + n * 2 * T
        w = 200 + (n * 37) % 1500
        h = 40 + (n * 13) % 120
        objs = [(1, 0, 100, 900, n % 7 == 0, None, w, h, 100 + n)]
        if n % 3 == 0:
            objs.append((2, 1, 100, 100, False, None, 300, 50, 400 + n))
        long_units.append(epoch(t, objs, number=2 * n, pal=palette(256, n)))
        long_units.append(clear(t + T, 2 * n + 1))
    add("long-movie", long_units, ("sup", "mkv", "ts"))
    add("long-movie-forced", long_units, ("sup",), ["opt:forced_subs_only=1"])
    return cases

def receiver_cases():
    """Packet-level receiver behaviour, in transport streams (one PES per packet,
    so several segments share a packet)."""
    T = 90000
    pal = [(1, 235, 128, 128, 255), (2, 16, 128, 128, 128)]

    def small(objs=((1, 0, 3, 0, False, None),), state=0x80, number=0, palette_id=0):
        return pcs(16, 4, number, state, list(objs), 0, palette_id)

    def obj(oid=1, w=2, h=1, data=b"\x01\x02\x00\x00"):
        return ods_parts(oid, 0, w, h, data)

    full = small(((1, 0, 3, 0, True, (0, 0, 1, 1)), (2, 0, 5, 0, False, None)))
    picture = (T, T, [small(), pds(0, 0, pal), *obj(), END])
    cases = []

    def add(name, units, options=()):
        cases.append((name, units, ("ts",), list(options)))

    # Packets shorter than a segment header are refused; the state stays.
    add("rx-short-packets", [(T, T, [small(), pds(0, 0, pal), *obj()]), (2 * T, 2 * T, [b"\x80"]),
                             (3 * T, 3 * T, [b"\x80\x00"]), (4 * T, 4 * T, [END])])
    # Trailing bytes: a partial header reads its length as zero; a segment
    # running past the packet ends it, except END.
    add("rx-tail-partial-header", [(T, T, [small(), pds(0, 0, pal), *obj(), b"\x80"]),
                                   (2 * T, 2 * T, [small(number=1), *obj(), b"\x16\x00"]),
                                   (3 * T, 3 * T, [END])])
    add("rx-tail-overrun", [picture, (2 * T, 2 * T, [full[:-3]]), (3 * T, 3 * T, [END]),
                            (4 * T, 4 * T, [pds(0, 1, pal[::-1])[:-1]]), (5 * T, 5 * T, [END]),
                            (6 * T, 6 * T, [b"\x80\x00\x05"]), (7 * T, 7 * T, [END, b"\x80\x00\x09\x00"])])
    # PCS fields past a short segment come from the following bytes.
    add("rx-pcs-reads-on", [picture] + [
        (t * T, t * T, [segment(0x16, full[3:3 + n]), *rest]) for t, (n, rest) in enumerate(
            [(2, [segment(0x15, b"\x00\x10\x00")]), (10, [END]), (7, [pds(0, 0, pal)]), (19, [])], 2)]
        + [(9 * T, 9 * T, [pds(0, 0, pal), END])])
    add("rx-pcs-truncated-sizes", [picture] + [
        (T * (2 + 2 * i), T * (2 + 2 * i), [segment(0x16, full[3:3 + n])])
        for i, n in enumerate([0, 3, 4, 8, 11, 19, 27])
    ] + [(T * (3 + 2 * i), T * (3 + 2 * i), [pds(0, 0, pal), END]) for i in range(7)])
    # PDS without an id or entries, and entries crossing into the next segment.
    add("rx-pds-reads-on", [(T, T, [small(palette_id=0x15), segment(0x14, b""), *obj(), END]),
                            (2 * T, 2 * T, [small(palette_id=0, number=1),
                                            segment(0x14, b"\x00\x00\x01\x10\x80\x80"),
                                            segment(0x18, b"\xff"), *obj(), END])])
    # ODS header variants: cut first fragment, declared length below its size
    # fields (refusal; earlier segments stay applied), data beyond the length.
    bad = segment(0x15, b"\x00\x05\x00\xc0\x00\x00\x03\x00\x02\x00\x01\x01\x02\x00\x00")
    add("rx-ods-headers", [
        (T, T, [small(), pds(0, 0, pal), segment(0x15, b"\x00\x01\x00\xc0\x00\x00\x08"), END]),
        (2 * T, 2 * T, [small(number=1), pds(0, 0, pal), segment(0x15, b"\x00\x01\x00"), END]),
        (3 * T, 3 * T, [small(number=2), pds(0, 0, pal), *obj(), END]),
        (4 * T, 4 * T, [small(((1, 0, 7, 0, False, None),), state=0, number=3), bad,
                        small(((1, 0, 11, 0, False, None),), state=0, number=4)]),
        (5 * T, 5 * T, [END]),
        (6 * T, 6 * T, [segment(0x15, b"\x00\x01\x00\xc0\x00\x00\x06\x00\x02\x00\x01\x01\x02\x00\x00"),
                        END]),
        (7 * T, 7 * T, [segment(0x15, b"\x00\x01\x00\xc0\x00\x00\x08\x00\x02\x00\x01\x01"), END])])
    # Displays: later ENDs in a packet are ignored; a missing palette returns
    # nothing and zeroes the packet's PTS; the presentation clock is used once.
    add("rx-displays", [
        (T, T, [small(), pds(0, 0, pal), *obj(), END, small(number=1), END]),
        (2 * T, 2 * T, [pds(0, 1, pal), END]),
        (3 * T, 3 * T, [small(palette_id=3, number=2), *obj(), END, pds(3, 0, pal), END]),
        (4 * T, 4 * T, [small(palette_id=3, number=3), *obj(), END]),
        (5 * T, 5 * T, [pds(3, 1, pal), END]),
        (6 * T, 6 * T, [END]),
        (None, None, [small(number=4), END]),
        (7 * T, None, [pds(3, 2, pal), END])])
    forced_missing = [(T, T, [small(((1, 0, 3, 0, True, None),)), pds(0, 0, pal), END]),
                      (2 * T, 2 * T, [small(((1, 0, 3, 0, False, None),), number=1), END]),
                      (3 * T, 3 * T, [small(((1, 0, 3, 0, True, None),), number=2),
                                      segment(0x15, b"\x00\x01\x00\xc0\x00\x00\x08"), END]),
                      (4 * T, 4 * T, [small(((1, 0, 3, 0, True, None),), number=3),
                                      *ods_parts(1, 0, 2, 1, b"\x00\x86\x01"), END])]
    add("rx-forced-empty-rects", forced_missing)
    add("rx-forced-empty-rects-only", forced_missing, ["opt:forced_subs_only=1"])
    # A PCS sets the presentation PTS from the subtitle PTS current at that
    # point of its packet, so one after a display keeps that display's PTS.
    add("rx-presentation-after-display", [
        (T, T, [small(), pds(0, 0, pal), *obj(), END, small((), state=0, number=1)]), (2 * T, 2 * T, [END]),
        (3 * T, 3 * T, [small(number=2), pds(0, 0, pal), *obj()]),
        (4 * T, 4 * T, [END, small((), state=0, number=3)]), (5 * T, 5 * T, [END]),
        (6 * T, 6 * T, [pds(0, 1, pal), END, small((), state=0, number=4)]), (7 * T, 7 * T, [END]),
        (8 * T, 8 * T, [small(number=5)]), (9 * T, 9 * T, [END])])
    # Only the two high state bits start an epoch.
    add("rx-epoch-states", [x for n, state in enumerate([0x01, 0x15, 0x20, 0x3f, 0x40, 0x41, 0x80, 0xc0])
                            for x in ((T * (2 * n + 1), T * (2 * n + 1), [small(number=2 * n), pds(0, 0, pal), *obj(), END]),
                                      (T * (2 * n + 2), T * (2 * n + 2), [small(state=state, number=2 * n + 1), END]))])
    # A rejected canvas zeroes the receiver's canvas (objects then no longer
    # fit, palettes use the HD matrix) but keeps the composition and its
    # positions, which each PCS places against its own canvas.
    hd = (1, 0, 10, 10, False, None)
    far = (1, 0, 700, 10, False, None)
    art = rle(pattern(100, 20, 17))
    pal3 = [(1, 100, 50, 200, 255)]
    four = ods_parts(1, 0, 4, 1, b"\x00\x84\x01\x00\x00")
    add("rx-rejected-canvas", [
        (T, T, [pcs(1280, 720, 0, 0x80, [hd]), pds(0, 0, palette()), END]),
        (2 * T, 2 * T, [pcs(0, 0, 1, 0x80, [hd]), pds(0, 0, palette()), *ods_parts(1, 0, 100, 20, art), END]),
        (3 * T, 3 * T, [pcs(1280, 720, 2, 0x80, [hd]), pds(0, 0, palette()), *ods_parts(1, 0, 100, 20, art), END]),
        (4 * T, 4 * T, [pcs(0, 0, 3, 0x80, [hd]), pds(0, 0, palette()), *ods_parts(1, 0, 100, 20, art), END]),
        (5 * T, 5 * T, [pcs(720, 480, 4, 0x80, [far]), pds(0, 0, pal3), *four, END]),
        (6 * T, 6 * T, [pcs(0, 0, 5, 0, []), END]),
        (7 * T, 7 * T, [pcs(0, 0, 6, 0, []), pds(0, 1, pal3), END]),
        (8 * T, 8 * T, [pcs(600, 480, 7, 0, [far]), END])])
    # First fragments declaring fewer than four bytes: refused only when the
    # fragment is otherwise acceptable.
    def short_ods(declared, width, height, data):
        return segment(0x15, b"\x00\x01\x00\xc0" + declared.to_bytes(3, "big") + struct.pack(">HH", width, height) + data)
    add("rx-ods-declared-length", [
        (T * (n + 1), T * (n + 1), [small(), pds(0, 0, pal), seg, small(((1, 0, 9, 0, False, None),), state=0, number=1), END])
        for n, seg in enumerate([short_ods(3, 0, 1, b"\x01"), short_ods(3, 2, 0, b"\x01"), short_ods(3, 2, 1, b""),
                                 short_ods(3, 17, 1, b"\x01"), short_ods(3, 2, 5, b"\x01"), short_ods(3, 2, 1, b"\x01"),
                                 short_ods(0, 2, 1, b"\x01"), short_ods(4, 2, 1, b"\x01"), short_ods(4, 2, 1, b"")])])
    add("rx-clock-before-composition", [(T, T, [END]), (2 * T, 2 * T, [pds(0, 0, pal), END]),
                                        (3 * T, 3 * T, [small(), *obj(), END])])
    # Run-length coding: one cursor across rows, overlong runs dropped.
    rle_cases = [b"\x01" * 6 + b"\x00\x00" + b"\x02" * 4 + b"\x00\x00",
                 b"\x01" * 4 + b"\x00\x86\x02" + b"\x03" * 4 + b"\x00\x00",
                 b"\x01" * 3 + b"\x00\x86\x02" + b"\x03" * 4 + b"\x00\x00",
                 b"\x00\x88\x01\x01\x01\x00\x00", b"\x00\x88\x01", b"\x01\x00\x00\x02\x00\x00",
                 b"\x00\x80\x05\x00\x88\x01\x00\x00", b"\x00\x40\x00\x00\x88\x01\x00\x00",
                 b"\x00\x87\x01\x00", b"\x00\x87\x01\x00\x80", b"\x00\x87\x01\x00\x81",
                 b"\x00\x87\x01\x00\xc0", b"\x00\x87\x01\x00\xc0\x01", b"\x00\x84\x01\x00\x86\x02\x00\x00"]
    add("rx-rle-plane", [(T * (i + 1), T * (i + 1), [small(state=0x80, number=i), pds(0, 0, pal),
                                                     *ods_parts(1, 0, 4, 2, data), END])
                         for i, data in enumerate(rle_cases)])
    return cases

# --------------------------------------------------------------- gold inputs


def gold_records(raw):
    """(flags, numerator, denominator, records) of one SMBMO001 transcript input."""
    assert raw[:8] == b"SMBMO001" and raw[8] == 1
    flags, num, den, size = struct.unpack(">IIII", raw[9:25])
    at = 25 + size
    (count,) = struct.unpack(">I", raw[at:at + 4])
    at += 4
    records = []
    for _ in range(count):
        reset, pk = raw[at], raw[at + 1]
        (pts,) = struct.unpack(">q", raw[at + 2:at + 10])
        dk = raw[at + 10]
        (dts,) = struct.unpack(">q", raw[at + 11:at + 19])
        (n,) = struct.unpack(">I", raw[at + 19:at + 23])
        at += 23
        records.append((reset, pts if pk else None, dts if dk else None, raw[at:at + n]))
        at += n
    return flags, num, den, size, records


def gold_cases():
    """Recorded packet inputs that fit a 90 kHz PES: no reopen records, no private
    configuration, PTS/DTS absent or within 0..2^33-1; one PES per record."""
    out, skipped = [], []
    for name in sorted(os.listdir(GOLD)):
        path = os.path.join(GOLD, name, "input.bin")
        if not os.path.isfile(path):
            continue
        flags, num, den, size, records = gold_records(open(path, "rb").read())
        reason = None
        if (num, den) != (1, 90000):
            reason = f"time base {num}/{den}"
        elif size:
            reason = "private configuration"
        elif any(r[0] for r in records):
            reason = "decoder reopen record"
        elif any(v is not None and not 0 <= v < 1 << 33 for r in records for v in r[1:3]):
            reason = "clock outside 0..2^33"
        elif any(r[1] is None and r[2] is not None for r in records):
            reason = "DTS without PTS"
        if reason:
            skipped.append((name, reason))
            continue
        units = [(pts, dts, [payload]) for _, pts, dts, payload in records]
        options = ["opt:forced_subs_only=1"] if flags & 1 else []
        out.append((name.replace("pgs-", "gold-", 1), units, ("ts",), options))
    return out, skipped

# --------------------------------------------------------------------- main


FATE = os.path.join(ROOT, ".research/fate-samples/sub/pgs_sub.sup")
REFERENCE = os.path.join(ROOT, "target/harness/ffmpeg-subtitles-worker")
SOURCE = "harness/subtitles/pgs_streams.py"


def fate_cases(corpus):
    """The FATE sample (copied into the corpus) and Matroska and M2TS muxes of
    it made with Homebrew ffmpeg (9.0.2) as an authoring tool:

        ffmpeg -v error -y -i pgs_sub.sup -map 0 -c copy pgs_sub.mkv
        ffmpeg -v error -y -i pgs_sub.sup -map 0 -c copy -f mpegts -mpegts_m2ts_mode 1 pgs_sub.m2ts
    """
    if not os.path.isfile(FATE):
        print("FATE sample missing: " + FATE)
        return []
    fate_dir = os.path.join(corpus, "fate/sub")
    os.makedirs(fate_dir, exist_ok=True)
    copy = os.path.join(fate_dir, "pgs_sub.sup")
    with open(FATE, "rb") as src, open(copy, "wb") as dst:
        dst.write(src.read())
    rows = [("fate-pgs_sub", "fate/sub/pgs_sub.sup", "fate-suite sub/pgs_sub.sup")]
    for ext, extra in (("mkv", []), ("m2ts", ["-f", "mpegts", "-mpegts_m2ts_mode", "1"])):
        rel = f"pgs/fate-pgs_sub.{ext}"
        subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", copy, "-map", "0", "-c", "copy", *extra,
                        os.path.join(corpus, rel)], check=True)
        rows.append((f"fate-pgs_sub-{ext}", rel, "fate-suite sub/pgs_sub.sup, muxed by ffmpeg 9.0.2"))
    return rows


def reference_fold(path, args):
    """The reference's sub-fold, or None when it refuses to prepare the input."""
    out = subprocess.run([REFERENCE, "sub-fold", path, *args], capture_output=True, text=True)
    if out.returncode != 0:
        return None
    fold = json.loads(out.stdout.strip().splitlines()[-1])
    return {k: fold[k] for k in ("packets", "bytes", "digest")}


def main():
    corpus = os.path.join(ROOT, ".research/subtitle-corpus")
    out_dir = os.path.join(corpus, "pgs")
    os.makedirs(out_dir, exist_ok=True)
    gold, skipped = gold_cases()
    rows = []
    for case in synthetic_cases() + gold:
        name, units, carriers, options = case[:4]
        for carrier in carriers:
            if carrier == "raw":
                data, ext = case[4], "sup"
            elif carrier == "sup":
                data, ext = write_sup(units), "sup"
            elif carrier == "mkv":
                data, ext = write_mkv(units), "mkv"
            else:
                data, ext = write_ts(units, carrier == "m2ts"), carrier
            file = f"{name}.{ext}"
            with open(os.path.join(out_dir, file), "wb") as f:
                f.write(data)
            source = (f"{SOURCE}; recorded input crates/subtitles/tests/native-compatibility-gold/"
                      f"{name.replace('gold-', 'pgs-', 1)}/input.bin") if name.startswith("gold-") else SOURCE
            rows.append((f"{name}-{ext}", f"pgs/{file}", "authored-synthetic", source, options))
    # Two PGS streams in one transport stream: the second selected by ordinal.
    def stream(x, seed):
        return [epoch(90000 * (n + 1), [(1, 0, x, 900, False, None, 300, 40, seed + n)], number=n)
                for n in range(4)]

    two = write_ts(stream(100, 50), second=stream(900, 90))
    with open(os.path.join(out_dir, "two-streams.ts"), "wb") as f:
        f.write(two)
    for ordinal in (0, 1):
        rows.append((f"two-streams-{ordinal}-ts", "pgs/two-streams.ts", "authored-synthetic", SOURCE,
                     [f"stream={ordinal}"]))
    for case_id, rel, source in fate_cases(corpus):
        rows.append((case_id, rel, "fetched-not-redistributed", source, []))
    cases, refused = [], []
    for case_id, rel, rights, source, options in rows:
        path = os.path.join(corpus, rel)
        with open(path, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        entry = {"id": case_id, "profile": case_id, "file": rel, "sha256": digest, "rights": rights,
                 "source": source, "arguments": ["pgs", *options]}
        expected = reference_fold(path, entry["arguments"]) if os.path.isfile(REFERENCE) else None
        if expected is None:
            refused.append(entry)
        else:
            cases.append({**entry, "expected": expected})
    manifest = {"schema": 1, "cases": cases,
                "refused": refused,
                "not_representable": [{"input": f"crates/subtitles/tests/native-compatibility-gold/{n}/input.bin",
                                       "reason": r} for n, r in skipped]}
    target = os.path.join(ROOT, "harness/features/subtitle-pgs/corpus.json")
    with open(target, "w") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
    print(f"{len(cases)} cases, {len(refused)} refused by the reference, "
          f"{len(skipped)} recorded inputs not representable; manifest {target}")


if __name__ == "__main__":
    main()
