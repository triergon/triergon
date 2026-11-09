#!/usr/bin/env python3
"""Deterministic DVB teletext transport streams for the subtitle corpus.

Writes MPEG-2 transport streams carrying EN 300 472 teletext PES (data
identifier 0x10, EBU teletext data units) with EN 300 706 packets: page headers,
display rows, X/26 enhancements and X/28 page enhancement packets. Every stream
is built from Python's standard library only, byte for byte the same on every
run.

usage: teletext_streams.py [OUTPUT_DIR]   (default: .research/subtitle-corpus/teletext)
       teletext_streams.py --manifest [REFERENCE_WORKER]
           writes the streams and harness/features/subtitle-teletext/corpus.json
           with the reference worker's sub-fold results (default:
           target/harness/ffmpeg-subtitles-worker)

The streams are a single program with a teletext stream (type 0x06, teletext
descriptor) and a PCR packet half a second before each PES, as FFmpeg's mpegts
demuxer times teletext by the PCR.
"""

import hashlib
import json
import os
import struct
import sys

# ---------------------------------------------------------------- EN 300 706 codes

HAM84 = [0x15, 0x02, 0x49, 0x5E, 0x64, 0x73, 0x38, 0x2F,
         0xD0, 0xC7, 0x8C, 0x9B, 0xA1, 0xB6, 0xFD, 0xEA]


def ham84(nibble):
    """Hamming 8/4 byte, bit b1 in the least significant position."""
    return HAM84[nibble & 15]


def parity(code):
    """Seven-bit code with odd parity in b8."""
    code &= 0x7F
    return code | (0 if bin(code).count("1") % 2 else 0x80)


def ham2418(value):
    """Hamming 24/18 triplet of an 18-bit value, three bytes b1 first."""
    data_positions = [3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23]
    bits = [0] * 25  # 1-based positions 1..24
    for i, pos in enumerate(data_positions):
        bits[pos] = (value >> i) & 1
    for p in (1, 2, 4, 8, 16):
        bits[p] = (sum(bits[k] for k in range(1, 24) if k & p and k != p) + 1) & 1  # odd parity
    bits[24] = (sum(bits[1:24]) + 1) & 1  # overall odd parity
    word = 0
    for pos in range(1, 25):
        word |= bits[pos] << (pos - 1)
    return bytes([word & 0xFF, (word >> 8) & 0xFF, (word >> 16) & 0xFF])


def mrag(magazine, row):
    m = magazine & 7
    return bytes([ham84(m | ((row & 1) << 3)), ham84(row >> 1)])


def text_bytes(text, width):
    """Odd-parity row bytes; `text` is a str of 7-bit codes or a bytes object."""
    if isinstance(text, str):
        codes = [ord(c) for c in text]
    else:
        codes = list(text)
    codes = (codes + [0x20] * width)[:width]
    return bytes(parity(c) for c in codes)


def header_packet(magazine, page, text="", subcode=0, erase=False, newsflash=False,
                  subtitle=False, suppress=False, update=False, interrupted=False,
                  inhibit=False, serial=False, national=0):
    """X/0 page header. `page` is the two-digit hex page within the magazine."""
    s1 = subcode & 0xF
    s2 = (subcode >> 4) & 0x7
    s3 = (subcode >> 8) & 0xF
    s4 = (subcode >> 12) & 0x3
    body = bytes([
        ham84(page & 0xF),
        ham84(page >> 4),
        ham84(s1),
        ham84(s2 | (int(erase) << 3)),
        ham84(s3),
        ham84(s4 | (int(newsflash) << 2) | (int(subtitle) << 3)),
        ham84(int(suppress) | (int(update) << 1) | (int(interrupted) << 2) | (int(inhibit) << 3)),
        ham84(int(serial) | ((national & 1) << 3) | ((national >> 1 & 1) << 2) | ((national >> 2 & 1) << 1)),
    ])
    return mrag(magazine, 0) + body + text_bytes(text, 32)


def row_packet(magazine, row, text):
    return mrag(magazine, row) + text_bytes(text, 40)


def raw_packet(magazine, row, body):
    body = bytes(body)
    assert len(body) == 40
    return mrag(magazine, row) + body


def triplet(address, mode, data):
    """X/26 triplet: 6-bit address, 5-bit mode, 7-bit data."""
    return ham2418((address & 0x3F) | ((mode & 0x1F) << 6) | ((data & 0x7F) << 11))


def enhancement_packet(magazine, designation, triplets, row=26):
    triplets = list(triplets)
    while len(triplets) < 13:
        triplets.append(triplet(0x3F, 0x1F, 0))  # termination marker
    return raw_packet(magazine, row, bytes([ham84(designation)]) + b"".join(triplets[:13]))


def x28_format1(magazine, designation=0, page_function=0, coding=0, default_g0g2=0,
                second_g0=0, left_panel=False, right_panel=False, panel_status=False,
                left_columns=0, clut=None, default_screen=0, default_row=0,
                black_bg_sub=False, colour_table=0):
    """X/28/0 format 1 (or X/28/4): the 13 triplets as one 234-bit field."""
    bits = []

    def put(value, width):
        for i in range(width):
            bits.append((value >> i) & 1)

    put(page_function, 4)
    put(coding, 3)
    put(default_g0g2, 7)
    put(second_g0, 7)
    put(int(left_panel), 1)
    put(int(right_panel), 1)
    put(int(panel_status), 1)
    put(left_columns, 4)
    clut = clut or [0] * 16
    for entry in clut:
        put(entry, 12)
    put(default_screen, 5)
    put(default_row, 5)
    put(int(black_bg_sub), 1)
    put(colour_table, 3)
    while len(bits) < 13 * 18:
        bits.append(0)
    triplets = []
    for t in range(13):
        value = 0
        for i in range(18):
            value |= bits[t * 18 + i] << i
        triplets.append(ham2418(value))
    return raw_packet(magazine, 28, bytes([ham84(designation)]) + b"".join(triplets))


# ---------------------------------------------------------------- EN 300 472 PES

def reverse(byte):
    return int(f"{byte:08b}"[::-1], 2)


def data_unit(packet, unit_id=0x03, field=1, line=7):
    assert len(packet) == 42
    head = bytes([unit_id, 0x2C, 0xC0 | (field << 5) | (line & 0x1F), 0xE4])
    return head + bytes(reverse(b) for b in packet)


STUFFING = bytes([0xFF, 0x2C]) + bytes([0xFF] * 44)


def pts_bytes(pts, marker=0x2):
    pts &= (1 << 33) - 1
    return bytes([
        (marker << 4) | ((pts >> 29) & 0x0E) | 1,
        (pts >> 22) & 0xFF,
        ((pts >> 14) & 0xFE) | 1,
        (pts >> 7) & 0xFF,
        ((pts << 1) & 0xFE) | 1,
    ])


def pes(units, pts=None, data_identifier=0x10, pad=True):
    """One EN 300 472 PES packet: a 45-byte header, the data identifier and units."""
    units = list(units)
    if pad:
        while (len(units) + 1) % 4:
            units.append(STUFFING)
    optional = pts_bytes(pts) if pts is not None else b""
    optional += bytes([0xFF] * (0x24 - len(optional)))
    flags = 0x80 if pts is not None else 0x00
    body = bytes([0x84, flags, 0x24]) + optional + bytes([data_identifier]) + b"".join(units)
    return b"\x00\x00\x01\xBD" + struct.pack(">H", len(body)) + body


# ---------------------------------------------------------------- MPEG-2 TS

def crc32_mpeg(data):
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) if crc & 0x80000000 else crc << 1
            crc &= 0xFFFFFFFF
    return crc


def section(table_id, extension, payload):
    length = 5 + len(payload) + 4
    head = bytes([table_id, 0xB0 | (length >> 8), length & 0xFF,
                  extension >> 8, extension & 0xFF, 0xC1, 0x00, 0x00])
    body = head + payload
    return body + struct.pack(">I", crc32_mpeg(body))


def teletext_descriptor(entries):
    """entries: (language, type, magazine, page) tuples."""
    data = b""
    for language, kind, magazine, page in entries:
        data += language.encode("ascii") + bytes([(kind << 3) | (magazine & 7), page])
    return bytes([0x56, len(data)]) + data


class Mux:
    """Single-program transport stream writer."""

    def __init__(self, streams, pmt_pid=0x1000, pcr_pid=0x1FFF):
        self.out = bytearray()
        self.cc = {}
        self.pmt_pid = pmt_pid
        self.pcr_pid = pcr_pid
        self.streams = streams  # [(pid, stream_type, descriptors)]
        self.tables()

    def packet(self, pid, payload, start):
        cc = self.cc.get(pid, 0)
        self.cc[pid] = (cc + 1) & 15
        head = bytes([0x47, (0x40 if start else 0) | (pid >> 8), pid & 0xFF])
        if len(payload) < 184:
            stuffing = 184 - len(payload)
            if stuffing == 1:
                adaptation = b"\x00"
            else:
                adaptation = bytes([stuffing - 1, 0x00]) + bytes([0xFF] * (stuffing - 2))
            self.out += head + bytes([0x30 | cc]) + adaptation + payload
        else:
            self.out += head + bytes([0x10 | cc]) + payload

    def psi(self, pid, data):
        self.packet(pid, b"\x00" + data, True)

    def tables(self):
        pat = section(0x00, 1, struct.pack(">HH", 1, 0xE000 | self.pmt_pid))
        info = b""
        for pid, stream_type, descriptors in self.streams:
            info += bytes([stream_type]) + struct.pack(">HH", 0xE000 | pid, 0xF000 | len(descriptors)) + descriptors
        pmt = section(0x02, 1, struct.pack(">HH", 0xE000 | self.pcr_pid, 0xF000) + info)
        self.psi(0, pat)
        self.psi(self.pmt_pid, pmt)

    def pcr(self, value):
        """An adaptation-only packet on the PCR PID carrying `value` (90 kHz)."""
        base = value & ((1 << 33) - 1)
        field = bytes([0x10, (base >> 25) & 0xFF, (base >> 17) & 0xFF, (base >> 9) & 0xFF,
                       (base >> 1) & 0xFF, ((base & 1) << 7) | 0x7E, 0x00])
        adaptation = bytes([183]) + field + bytes([0xFF] * (183 - len(field)))
        cc = self.cc.get(self.pcr_pid, 0)
        pid = self.pcr_pid
        self.out += bytes([0x47, pid >> 8, pid & 0xFF, 0x20 | cc]) + adaptation

    def pes(self, pid, data):
        for at in range(0, len(data), 184):
            self.packet(pid, data[at:at + 184], at == 0)

    def bytes(self):
        return bytes(self.out)


# ---------------------------------------------------------------- streams

BOX = "\x0b\x0b"
END = "\x0a\x0a"
SUBTITLE = dict(erase=True, subtitle=True, suppress=True)
CLOCK = 90000  # one second at 90 kHz
PCR_LEAD = 45000  # PCR half a second before the PTS, as broadcasts send it


class Stream:
    """Teletext PES packets on one or more PIDs, each after a PCR packet."""

    def __init__(self, languages=(("eng", 2, 8, 0x88),), pids=None):
        pids = pids or {0x100: list(languages)}
        self.pids = list(pids)
        self.mux = Mux([(pid, 0x06, teletext_descriptor(entries)) for pid, entries in pids.items()],
                       pcr_pid=0x1FF)

    def send(self, pts, packets, pid=None, unit_id=0x03, pcr=True, data_identifier=0x10, pad=True):
        if pcr and pts is not None:
            self.mux.pcr(pts - PCR_LEAD)
        units = [data_unit(p, unit_id, 1, 7 + i % 16) for i, p in enumerate(packets)]
        self.mux.pes(pid or self.pids[0], pes(units, pts, data_identifier, pad))

    def raw(self, pts, payload, pid=None):
        """A PES packet with `payload` after the 45-byte header."""
        self.mux.pcr(pts - PCR_LEAD)
        optional = pts_bytes(pts) + bytes([0xFF] * 31)
        body = bytes([0x84, 0x80, 0x24]) + optional + payload
        self.mux.pes(pid or self.pids[0], b"\x00\x00\x01\xBD" + struct.pack(">H", len(body)) + body)

    def bytes(self):
        return self.mux.bytes()


def subtitle_page(magazine, page, rows, **flags):
    """A subtitle page and the time-filling header that ends it."""
    flags = {**SUBTITLE, **flags}
    packets = [header_packet(magazine, page, "", **flags)]
    packets += [row_packet(magazine, row, text) for row, text in rows]
    return packets + [header_packet(magazine, 0xFF, "")]


def double(text, colour=""):
    """A double-height boxed subtitle line."""
    return "\x0d" + colour + BOX + text + END


def centred(text, colour=""):
    pad = max(0, (36 - len(text)) // 2)
    return " " * pad + double(text, colour)


def case_basic():
    s = Stream()
    lines = [
        [(20, centred("Good evening."))],
        [(20, centred("This is the news")), (22, centred("at nine o'clock."))],
        [],
        [(22, centred("[MUSIC]", "\x06"))],
        [(20, centred("- Where are you going?")), (22, centred("- Home.", "\x03"))],
        [],
    ]
    for i, rows in enumerate(lines):
        s.send(CLOCK * (10 + 2 * i), subtitle_page(8, 0x88, rows))
    return s.bytes()


def case_layout():
    s = Stream()
    pages = [
        [(2, BOX + "Top left")],
        [(3, " " * 12 + BOX + "Top centre" + END), (5, " " * 14 + BOX + "two" + END)],
        [(10, " " * 26 + BOX + "Middle right")],
        [(12, " " * 8 + BOX + "Centred block" + END), (14, " " * 10 + BOX + "under" + END)],
        [(20, BOX + "Bottom left"), (22, BOX + "second")],
        [(21, " " * 28 + BOX + "bottom"), (23, " " * 30 + BOX + "right")],
        [(5, " " * 6 + BOX + "far apart"), (21, " " * 6 + BOX + "rows")],
        [(19, " " * 10 + BOX + "one  three" + END), (20, " " * 10 + BOX + "two" + END)],
    ]
    for i, rows in enumerate(pages):
        s.send(CLOCK * (5 + i), subtitle_page(8, 0x88, rows))
    return s.bytes()


def case_colours():
    s = Stream()
    pages = [
        [(20, BOX + "\x01Red \x02Green \x03Yellow \x04Blue"), (22, BOX + "\x05Magenta \x06Cyan \x07White")],
        [(20, BOX + "\x01\x1d\x07White on red\x1c black")],
        [(21, " " * 5 + "\x03" + BOX + "yellow line one" + END), (23, " " * 5 + BOX + "white line two" + END)],
        [(20, "\x06" + BOX + "cyan \x08flash\x09 steady \x18concealed")],
        [(22, BOX + "\x04\x1d\x03yellow on blue\x1c" + END)],
    ]
    for i, rows in enumerate(pages):
        s.send(CLOCK * (3 + 2 * i), subtitle_page(8, 0x88, rows))
    return s.bytes()


def case_sizes():
    s = Stream()
    pages = [
        [(20, " " * 8 + "\x0d" + BOX + "double height" + END)],
        [(20, " " * 8 + "\x0e" + BOX + "double width" + END)],
        [(18, " " * 4 + "\x0f" + BOX + "double size" + END)],
        [(20, BOX + "normal \x0dtall\x0c normal"), (21, BOX + "hidden lower row")],
        [(23, "\x0d" + BOX + "no double height on row 23"), (24, BOX + "row 24")],
    ]
    for i, rows in enumerate(pages):
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x88, rows))
    return s.bytes()


def case_national():
    s = Stream()
    sample = "#$@[\\]^_`{|}~ Gruesse"
    for national in range(8):
        s.send(CLOCK * (2 + national),
               subtitle_page(8, 0x88, [(20, BOX + sample), (22, BOX + "national %d" % national)],
                             national=national))
    # Character sets designated by X/28/0 and M/29/0.
    s.send(CLOCK * 11, [header_packet(8, 0x88, "", national=0, **SUBTITLE),
                        x28_format1(8, 0, default_g0g2=8, second_g0=32),
                        row_packet(8, 20, BOX + sample + " \x1bAbc\x1b"),
                        header_packet(8, 0xFF, "")])
    m29 = bytearray(x28_format1(8, 0, default_g0g2=36))
    m29[:2] = mrag(8, 29)
    s.send(CLOCK * 12, [bytes(m29)] + subtitle_page(8, 0x88, [(20, BOX + "Privet " + sample)]))
    return s.bytes()


def case_x26():
    s = Stream()
    accents = [  # (column, diacritical mark, letter)
        (2, 0x12, ord("e")), (3, 0x11, ord("a")), (4, 0x13, ord("o")), (5, 0x18, ord("u")),
        (6, 0x1F, ord("c")), (7, 0x1B, ord("c")), (8, 0x14, ord("n")), (9, 0x1A, ord("a")),
    ]
    trips = [triplet(40 + 20, 0x04, 0)] + [triplet(c, m, d) for c, m, d in accents]
    trips += [triplet(40 + 22, 0x04, 2), triplet(2, 0x0F, 0x23), triplet(3, 0x0F, 0x69),
              triplet(4, 0x0F, 0x7B), triplet(6, 0x0F, 0x50)]
    rows = [(20, BOX + "........ accents"), (22, BOX + ".... G2 set")]
    s.send(CLOCK * 2, [header_packet(8, 0x88, "", **SUBTITLE), enhancement_packet(8, 0, trips)]
           + [row_packet(8, r, t) for r, t in rows] + [header_packet(8, 0xFF, "")])
    # A mark Unicode cannot compose on a subtitle page and on a normal page.
    bad = [triplet(40 + 20, 0x04, 2), triplet(3, 0x12, ord("q"))]
    s.send(CLOCK * 3, [header_packet(8, 0x88, "", **SUBTITLE), enhancement_packet(8, 0, bad),
                       row_packet(8, 20, BOX + "xxxx boxed"), row_packet(8, 22, "unboxed"),
                       header_packet(8, 0xFF, "")])
    s.send(CLOCK * 4, [header_packet(1, 0x00, "INDEX", erase=True), enhancement_packet(1, 0, bad),
                       row_packet(1, 2, "before"), row_packet(1, 20, "xxxx cut"), row_packet(1, 22, "after"),
                       header_packet(1, 0xFF, "")])
    return s.bytes()


def case_erase_update():
    s = Stream()
    s.send(CLOCK * 2, subtitle_page(8, 0x88, [(20, BOX + "first line"), (22, BOX + "second line")]))
    s.send(CLOCK * 3, subtitle_page(8, 0x88, [(22, BOX + "replaced line")], erase=False, update=True))
    s.send(CLOCK * 4, subtitle_page(8, 0x88, [(21, BOX + "erased")], interrupted=True))
    s.send(CLOCK * 5, subtitle_page(8, 0x88, [(20, BOX + "subpage one")], subcode=1))
    s.send(CLOCK * 6, subtitle_page(8, 0x88, [(20, BOX + "subpage two")], subcode=2))
    s.send(CLOCK * 7, subtitle_page(8, 0x88, [(22, BOX + "one again")], subcode=1, erase=False))
    # A header repeated before the page ends restarts the page.
    s.send(CLOCK * 8, [header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 20, BOX + "lost"),
                       header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 21, BOX + "kept"),
                       header_packet(8, 0xFF, "")])
    s.send(CLOCK * 9, subtitle_page(8, 0x88, [(20, BOX + "inhibited")], inhibit=True))
    return s.bytes()


def case_page_changes():
    s = Stream(languages=(("eng", 2, 8, 0x88), ("deu", 2, 8, 0x89)))
    for i in range(6):
        page = 0x88 if i % 2 == 0 else 0x89
        s.send(CLOCK * (2 + i), subtitle_page(8, page, [(20, BOX + "page 8%02x number %d" % (page, i))]))
    # Two pages in one packet, the second ended by the next packet's header.
    s.send(CLOCK * 9, [header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 20, BOX + "888 again"),
                       header_packet(8, 0x89, "", **SUBTITLE), row_packet(8, 20, BOX + "889 again")])
    s.send(CLOCK * 10, [header_packet(8, 0xFF, "")])
    return s.bytes()


def case_magazines():
    s = Stream(languages=(("eng", 2, 8, 0x88), ("eng", 1, 1, 0x00)))
    for i in range(4):
        s.send(CLOCK * (2 + i), [
            header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 20, BOX + "magazine 8 #%d" % i),
            header_packet(1, 0x00, "INDEX", erase=True), row_packet(1, 2, "Index page %d" % i),
            header_packet(2, 0x01, "NEWS", erase=True), row_packet(2, 3, "\x01News\x07 page"),
            row_packet(8, 22, BOX + "late row of 888"),
            header_packet(8, 0xFF, ""), header_packet(1, 0xFF, ""), header_packet(2, 0xFF, ""),
        ])
    return s.bytes()


def case_serial():
    s = Stream()
    for i in range(4):
        s.send(CLOCK * (2 + i), [
            header_packet(8, 0x88, "", serial=True, **SUBTITLE), row_packet(8, 20, BOX + "serial %d" % i),
            header_packet(1, 0x00, "INDEX", erase=True, serial=True), row_packet(1, 2, "index"),
            row_packet(8, 22, BOX + "dropped: its page has ended"),
        ])
    s.send(CLOCK * 7, [header_packet(1, 0xFF, "", serial=True)])
    return s.bytes()


def case_normal_pages():
    s = Stream(languages=(("eng", 1, 1, 0x00),))
    mosaic = "\x12" + "".join(chr(c) for c in range(0x20, 0x40)) + "\x07"
    for i in range(3):
        s.send(CLOCK * (2 + i), [
            header_packet(1, 0x00, "TELETEXT %d" % i, erase=True), row_packet(1, 1, "\x0dHeadlines"),
            row_packet(1, 4, "\x03Weather\x07 and \x06travel"), row_packet(1, 6, mosaic),
            row_packet(1, 7, "\x19\x14\x1ehold" + "\x1a!\"#\x1f\x07text"), row_packet(1, 24, "\x01Red \x02Green"),
            header_packet(1, 0x01, "SECOND", erase=True, suppress=True), row_packet(1, 3, "page 101"),
            header_packet(1, 0x0A, "HEX", erase=True), row_packet(1, 3, "hex page, not shown"),
            header_packet(1, 0xFF, ""),
        ])
    return s.bytes()


def case_header_only():
    s = Stream()
    s.send(CLOCK * 2, subtitle_page(8, 0x88, [(20, BOX + "shown")]))
    for i in range(3):
        s.send(CLOCK * (3 + i), [header_packet(8, 0x88, "", **SUBTITLE)])
    s.send(CLOCK * 6, [header_packet(8, 0xFF, "")])
    s.send(CLOCK * 7, [header_packet(8, 0x88, "", **SUBTITLE), header_packet(8, 0xFF, "")])
    return s.bytes()


def case_timing():
    s = Stream()
    # No PCR yet: the demuxer gives these packets no time.
    s.send(CLOCK * 2, subtitle_page(8, 0x88, [(20, BOX + "before the clock")]), pcr=False)
    s.send(CLOCK * 5, subtitle_page(8, 0x88, [(20, BOX + "after a gap")]))
    s.mux.pcr(CLOCK * 6)
    s.send(CLOCK * 6 + 3000, subtitle_page(8, 0x88, [(20, BOX + "PTS just after the PCR")]), pcr=False)
    s.send(CLOCK * 30, subtitle_page(8, 0x88, [(20, BOX + "long gap")]))
    s.send(CLOCK * 31, subtitle_page(8, 0x88, [(20, BOX + "PTS far ahead of the PCR")]), pcr=False)
    s.send(CLOCK * 32, [header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 20, BOX + "split")])
    s.send(CLOCK * 33, [row_packet(8, 22, BOX + "over two packets")])
    s.send(CLOCK * 34, [header_packet(8, 0xFF, "")])
    return s.bytes()


def case_two_streams():
    s = Stream(pids={
        0x100: [("eng", 2, 8, 0x88), ("eng", 5, 8, 0x89)],
        0x101: [("deu", 2, 8, 0x88)],
    })
    for i in range(4):
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x88, [(20, BOX + "English %d" % i)]), pid=0x100)
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x89, [(20, BOX + "English HoH %d" % i)]), pid=0x100)
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x88, [(20, BOX + "Deutsch %d" % i)], national=1), pid=0x101)
    return s.bytes()


def case_queue():
    s = Stream(languages=(("eng", 1, 1, 0x00),))
    packets = []
    for i in range(30):
        packets += [header_packet(1, int("%02d" % (10 + i), 16), "", erase=True), row_packet(1, 2, "page %d" % i)]
    packets.append(header_packet(1, 0xFF, ""))
    s.send(CLOCK * 2, packets[:64])
    s.send(CLOCK * 3, packets[64:])
    for i in range(3):
        s.send(CLOCK * (4 + i), [header_packet(1, 0xFF, "")])
    return s.bytes()


def case_malformed():
    s = Stream()
    good = subtitle_page(8, 0x88, [(20, BOX + "good")])
    s.send(CLOCK * 2, good)
    units = b"".join(data_unit(p) for p in good)
    s.raw(CLOCK * 3, bytes([0x10]) + units)                      # not a whole PES
    s.raw(CLOCK * 4, bytes([0x10]) + units[:46] + bytes([0x03, 0x2B]) + units[48:])  # bad length
    s.send(CLOCK * 5, good, data_identifier=0x20)                 # other data
    s.send(CLOCK * 6, good, data_identifier=0x99)                 # EN 301 775 teletext
    s.send(CLOCK * 7, good, unit_id=0x02)                         # non-subtitle units
    s.send(CLOCK * 8, good, unit_id=0xC0)                         # inverted teletext, ignored
    damaged = [bytearray(p) for p in subtitle_page(8, 0x88, [(20, BOX + "one bit"), (22, BOX + "parity")])]
    damaged[0][3] ^= 0x04                                          # corrected header error
    damaged[2][9] ^= 0x80                                          # parity error drops the row
    s.send(CLOCK * 9, [bytes(p) for p in damaged])
    broken = bytearray(header_packet(8, 0x89, "", **SUBTITLE))
    broken[3] ^= 0x03                                              # uncorrectable page tens
    s.send(CLOCK * 10, [header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 20, BOX + "discarded"),
                        bytes(broken), header_packet(8, 0xFF, "")])
    s.send(CLOCK * 11, subtitle_page(8, 0x88, [(20, BOX + "last")]))
    return s.bytes()


def m29_format1(magazine, designation=0, **fields):
    """M/29/0 or M/29/4: the format 1 fields for the whole magazine."""
    return mrag(magazine, 29) + x28_format1(magazine, designation, **fields)[2:]


def case_subtitle_alignment():
    """Subtitle-style placement: double-height lines, centring and its carry."""
    s = Stream()
    pages = [
        [(20, centred("centred double")), (22, centred("second line"))],
        [(20, "  " + double("left double")), (22, "  " + double("left again"))],
        [(21, " " * 8 + BOX + "x" * 21), (22, " " * 9 + BOX + "x" * 18)],
        [(21, " " * 8 + BOX + "x" * 21), (22, " " * 9 + BOX + "x" * 19)],
        [(2, "\x0d" + BOX + "top double" + END), (12, BOX + "middle")],
    ]
    for i, rows in enumerate(pages):
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x88, rows))
    return s.bytes()


def case_extra_column():
    """libzvbi's 41st column: header, double width, mosaics and enhancements."""
    s = Stream(languages=(("eng", 1, 1, 0x00),))
    rows = [(2, "x" + " " * 33 + "\x0e\x01ab\x02c"), (3, "\x11" + " " * 36 + "\x7f\x7f"),
            (4, " " * 37 + "\x12\x1a\x7f"), (5, " \x13\x14chC" + " " * 30 + "\x07")]
    s.send(CLOCK * 2, [header_packet(1, 0x00, "INDEX \x01RED", erase=True)]
           + [row_packet(1, r, t) for r, t in rows] + [header_packet(1, 0xFF, "")])
    x26 = enhancement_packet(1, 0, [triplet(45, 0x04, 38), triplet(39, 0x02, 0x26), triplet(38, 0x03, 4)])
    s.send(CLOCK * 3, [header_packet(1, 0x00, "INDEX", erase=True), x26]
           + [row_packet(1, r, t) for r, t in rows[1:]] + [header_packet(1, 0xFF, "")])
    return s.bytes()


def case_subtitle_restart():
    """Subtitle pages repeated with and without C4 and the flags of restarts."""
    s = Stream()
    s.send(CLOCK * 2, subtitle_page(8, 0x88, [(20, BOX + "first"), (22, BOX + "kept")]))
    s.send(CLOCK * 3, subtitle_page(8, 0x88, [(20, BOX + "update")], erase=False))
    s.send(CLOCK * 4, [header_packet(8, 0x88, "", **SUBTITLE), row_packet(8, 21, BOX + "restart"),
                       header_packet(8, 0x88, "", subtitle=True, suppress=True),
                       row_packet(8, 22, BOX + "after"), header_packet(8, 0xFF, "")])
    s.send(CLOCK * 5, subtitle_page(8, 0x88, [(23, BOX + "newsflash")], erase=False, newsflash=True))
    s.send(CLOCK * 6, subtitle_page(8, 0x88, [(20, BOX + "plain")], erase=False, subtitle=False))
    return s.bytes()


def case_subcodes():
    """Page memory across subcodes 0000, 3F7F and real subpages."""
    s = Stream()
    sequence = [(0x0001, True, "one"), (0x0000, True, "zero"), (0x0001, False, "one again"),
                (0x3F7F, True, "any"), (0x0000, False, "zero again"), (0x0002, True, "two"),
                (0x0000, False, "zero last"), (0x0002, False, "two again"), (0x3F7F, False, "any last")]
    for i, (subcode, erase, text) in enumerate(sequence):
        s.send(CLOCK * (2 + i), subtitle_page(8, 0x88, [(20 + i % 3, BOX + text)], subcode=subcode,
                                             erase=erase))
    return s.bytes()


def case_level25():
    """X/26 colours, display attributes, characters and their reach."""
    s = Stream(languages=(("eng", 1, 1, 0x00),))
    rows = [(1, "row one abcdefghijklmnopqrstuvwxyz"), (2, "colours \x01red\x1d\x02gr\x1c"),
            (3, "boxes \x0b\x0bin\x0a\x0aout ab\x0e\x01dw"), (4, "\x0d\x03double height"),
            (6, "abc\x11\x7f\x7f\x07def")]
    trips = [triplet(41, 0x04, 0), triplet(12, 0x00, 4), triplet(63, 0x07, 0), triplet(14, 0x10, 0x41),
             triplet(42, 0x04, 8), triplet(8, 0x03, 17), triplet(12, 0x00, 9), triplet(20, 0x0C, 0x10),
             triplet(43, 0x04, 2), triplet(2, 0x0C, 0x02), triplet(30, 0x00, 21), triplet(31, 0x01, 0x41),
             triplet(32, 0x01, 0x63), triplet(33, 0x02, 0x26), triplet(34, 0x08, 0x37), triplet(35, 0x09, 0x66),
             triplet(36, 0x03, 0x29), triplet(37, 0x0F, 0x41), triplet(44, 0x04, 3), triplet(4, 0x03, 2),
             triplet(45, 0x04, 1), triplet(46, 0x01, 0), triplet(3, 0x00, 8), triplet(5, 0x0E, 1)]
    s.send(CLOCK * 2, [header_packet(1, 0x00, "INDEX", erase=True), enhancement_packet(1, 0, trips[:13]),
                       enhancement_packet(1, 1, trips[13:])] + [row_packet(1, r, t) for r, t in rows]
           + [header_packet(1, 0xFF, "")])
    # Unpresentable enhancements leave the Level 1 page; damage ends them.
    drcs = enhancement_packet(1, 0, [triplet(41, 0x04, 0), triplet(2, 0x10, 0x41), triplet(4, 0x0D, 0x01)])
    s.send(CLOCK * 3, [header_packet(1, 0x00, "INDEX", erase=True, suppress=True), drcs,
                       row_packet(1, 1, "drcs page"), header_packet(1, 0xFF, "")])
    damaged = bytearray(enhancement_packet(1, 0, [triplet(41, 0x04, 0), triplet(1, 0x10, 0x41),
                                                  triplet(3, 0x10, 0x42), triplet(5, 0x10, 0x43)]))
    damaged[3 + 3 * 2 + 2] ^= 0x03
    s.send(CLOCK * 4, [header_packet(1, 0x00, "INDEX", erase=True), bytes(damaged),
                       row_packet(1, 1, "abcdefgh"), header_packet(1, 0xFF, "")])
    # Transparent black on transparent black disappears at Level 2.5.
    s.send(CLOCK * 5, [header_packet(1, 0x00, "INDEX", erase=True), x28_format1(1, 0, colour_table=3),
                       enhancement_packet(1, 0, [triplet(63, 0x1F, 0)]),
                       row_packet(1, 3, "ab\x00cd\x07ef"), header_packet(1, 0xFF, "")])
    return s.bytes()


def case_colour_tables():
    """X/28 and M/29 colour tables, remapping and the packets a page takes."""
    s = Stream(languages=(("eng", 1, 1, 0x00),))
    a = [0xA00 | i * 0x11 for i in range(16)]
    b = [0xB00 | i * 0x11 for i in range(16)]
    probe = enhancement_packet(1, 0, [triplet(41, 0x04, 0)] + [triplet(2 + 2 * c, 0x00, v) for c, v in
                                                               enumerate([1, 9, 13, 14, 17, 25, 8, 7])])
    row = (1, "a b c d e f g h i")
    codes = (2, "\x01ab\x1dcd\x07ef\x1c\x00gh\x12ij")

    def page(*extra, erase=True):
        return ([header_packet(1, 0x00, "INDEX", erase=erase)] + list(extra) + [probe]
                + [row_packet(1, r, t) for r, t in (row, codes)] + [header_packet(1, 0xFF, "")])

    s.send(CLOCK * 2, page())
    s.send(CLOCK * 3, page(x28_format1(1, 0, clut=a, colour_table=5)))
    s.send(CLOCK * 4, page(x28_format1(1, 4, clut=b, colour_table=7)))
    s.send(CLOCK * 5, [m29_format1(1, 0, clut=b, colour_table=2)] + page())
    s.send(CLOCK * 6, [m29_format1(1, 4, clut=a, colour_table=6, default_g0g2=0x20)] + page())
    s.send(CLOCK * 7, page(x28_format1(1, 4, clut=b, colour_table=3)))
    s.send(CLOCK * 8, [m29_format1(1, 4, clut=b)] + page(erase=False))
    s.send(CLOCK * 9, page(x28_format1(1, 0, clut=a, page_function=1)))
    return s.bytes()


def case_random(seed):
    """Random rows, attributes and enhancements on a few pages."""
    import random
    rng = random.Random(seed)
    s = Stream(languages=(("eng", 2, 8, 0x88), ("eng", 1, 1, 0x00)))
    pages = [(8, 0x88), (8, 0x89), (1, 0x00), (1, 0x01)]
    for i in range(24):
        packets = []
        for _ in range(rng.randint(1, 3)):
            magazine, page = rng.choice(pages)
            flags = dict(erase=rng.random() < 0.8, subtitle=rng.random() < 0.6, suppress=rng.random() < 0.6,
                         national=rng.choice([0, 0, 1, 2, 4]))
            packets.append(header_packet(magazine, page, "PAGE", **flags))
            for _ in range(rng.randint(0, 4)):
                row = [0x20] * rng.randint(0, 12) + [0x0B, 0x0B]
                while len(row) < 33:
                    r = rng.random()
                    row.append(rng.choice(b"abcdefgh ijk ABC .,") if r < 0.75 else rng.randint(0x01, 0x07)
                               if r < 0.9 else rng.choice([0x0A, 0x0D, 0x1D, 0x1C]))
                row += [0x20] * (36 - len(row)) + [0x0C, 0x07, 0x1C, 0x20]
                packets.append(row_packet(magazine, rng.randint(1, 23), bytes(row)))
            packets.append(header_packet(magazine, 0xFF, ""))
        s.send(CLOCK * (2 + i), packets)
    return s.bytes()


CASES = {
    "basic": case_basic,
    "layout": case_layout,
    "colours": case_colours,
    "sizes": case_sizes,
    "national": case_national,
    "x26": case_x26,
    "erase-update": case_erase_update,
    "page-changes": case_page_changes,
    "magazines": case_magazines,
    "serial": case_serial,
    "normal-pages": case_normal_pages,
    "header-only": case_header_only,
    "timing": case_timing,
    "two-streams": case_two_streams,
    "queue": case_queue,
    "malformed": case_malformed,
    "subtitle-alignment": case_subtitle_alignment,
    "extra-column": case_extra_column,
    "subtitle-restart": case_subtitle_restart,
    "subcodes": case_subcodes,
    "level25": case_level25,
    "colour-tables": case_colour_tables,
    "random-1": lambda: case_random(1),
    "random-2": lambda: case_random(2),
}

# Decoder arguments beyond the format, per case: (suffix, [arguments]).
VARIANTS = {
    "basic": [("", []), ("duration", ["opt:txt_duration=3000"]), ("raw", ["opt:txt_chop_spaces=0"])],
    "layout": [("", [])],
    "colours": [("", [])],
    "sizes": [("", [])],
    "national": [("", []), ("polish", ["opt:txt_default_region=8"]), ("cyrillic", ["opt:txt_default_region=36"])],
    "x26": [("", [])],
    "erase-update": [("", [])],
    "page-changes": [("", []), ("889", ["opt:txt_page=889"]), ("subtitles", ["opt:txt_page=subtitle"])],
    "magazines": [("", []), ("100", ["opt:txt_page=100"])],
    "serial": [("", [])],
    "normal-pages": [("", []), ("header", ["opt:txt_chop_top=0"])],
    "header-only": [("", [])],
    "timing": [("", [])],
    "two-streams": [("", []), ("second", ["stream=1"])],
    "queue": [("", [])],
    "malformed": [("", [])],
    "subtitle-alignment": [("", [])],
    "extra-column": [("", []), ("header", ["opt:txt_chop_top=0"])],
    "subtitle-restart": [("", [])],
    "subcodes": [("", [])],
    "level25": [("", []), ("header", ["opt:txt_chop_top=0"])],
    "colour-tables": [("", [])],
    "random-1": [("", [])],
    "random-2": [("", [])],
}


def write_streams(directory):
    os.makedirs(directory, exist_ok=True)
    written = {}
    for name, build in CASES.items():
        data = build()
        path = os.path.join(directory, name + ".ts")
        with open(path, "wb") as f:
            f.write(data)
        written[name] = hashlib.sha256(data).hexdigest()
    return written


def write_manifest(directory, reference, manifest):
    """Records the reference worker's `sub-fold` result of every case variant."""
    import subprocess
    hashes = write_streams(directory)
    cases = []
    for name, variants in VARIANTS.items():
        for fmt in ("text", "ass"):
            for suffix, extra in variants:
                arguments = ["teletext", "opt:txt_format=" + fmt] + extra
                path = os.path.join(directory, name + ".ts")
                out = subprocess.run([reference, "sub-fold", path] + arguments,
                                     capture_output=True, text=True, check=True).stdout
                fold = json.loads(out.splitlines()[-1])
                case_id = "-".join(p for p in ("teletext", name, fmt, suffix) if p)
                cases.append({
                    "id": case_id,
                    "profile": "teletext-" + name,
                    "file": "teletext/" + name + ".ts",
                    "sha256": hashes[name],
                    "rights": "authored-synthetic",
                    "source": "harness/subtitles/teletext_streams.py",
                    "arguments": arguments,
                    "expected": {k: fold[k] for k in ("packets", "bytes", "digest")},
                })
    with open(manifest, "w") as f:
        json.dump({"schema": 1, "cases": cases}, f, indent=2)
        f.write("\n")


def main(argv):
    """Writes the streams; `--manifest [REFERENCE]` also records the corpus,
    `--fuzz KIND START COUNT [REFERENCE CANDIDATE]` compares fuzz streams."""
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    directory = os.path.join(root, ".research", "subtitle-corpus", "teletext")
    if len(argv) > 4 and argv[1] == "--fuzz":
        import teletext_fuzz
        kind, start, count = argv[2], int(argv[3]), int(argv[4])
        workers = argv[5:7] or [os.path.join(root, "target", "harness", "ffmpeg-subtitles-worker"),
                                os.path.join(root, "target", "release", "subtitle-worker")]
        failures = teletext_fuzz.run(kind, start, count, *workers,
                                     os.path.join(root, ".research", "subtitle-corpus", "teletext-fuzz"))
        print("%s %d-%d: %d of %d differ %s" % (kind, start, start + count - 1, len(failures), 2 * count,
                                               " ".join("%d/%s" % f for f in failures)))
    elif len(argv) > 1 and argv[1] == "--manifest":
        reference = argv[2] if len(argv) > 2 else os.path.join(root, "target", "harness",
                                                                "ffmpeg-subtitles-worker")
        write_manifest(directory, reference,
                       os.path.join(root, "harness", "features", "subtitle-teletext", "corpus.json"))
    else:
        for name, digest in write_streams(argv[1] if len(argv) > 1 else directory).items():
            print(digest, name + ".ts")


if __name__ == "__main__":
    main(sys.argv)
