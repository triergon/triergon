#!/usr/bin/env python3
"""Deterministic DVD subpicture streams for the subtitle corpus.

Builds DVD-Video subpicture units (SPUs: 2-bit interlaced run-length pixels and
a display control sequence table), carries them in MPEG-2 program streams
(ISO/IEC 13818-1 pack headers and private_stream_1 PES packets with substream
id 0x20+N, one 2048-byte sector per pack), and writes VobSub `.idx`/`.sub`
pairs, raw `.vob` program streams and a VTS IFO file with a PGC colour table.
Matroska and MP4 copies are muxed with Homebrew `ffmpeg` (9.0.2) as an
authoring tool:

    ffmpeg -v error -y -i NAME.idx -map 0 -c copy NAME.mkv
    ffmpeg -v error -y -i NAME.idx -map 0 -c copy NAME.mp4

Everything else is Python's standard library, byte for byte the same on every
run (the muxed copies depend on that ffmpeg build; their hashes are recorded in
the corpus manifest).

usage: dvd_streams.py [OUTPUT_DIR]   (default: .research/subtitle-corpus/dvd)
"""

import hashlib
import json
import os
import shutil
import struct
import subprocess
import sys
import zlib

SECTOR = 2048

# ------------------------------------------------------------- 2-bit RLE pixels


class Nibbles:
    """A big-endian nibble writer."""

    def __init__(self):
        self.out = bytearray()
        self.half = None

    def put(self, value, count):
        for shift in range(4 * (count - 1), -1, -4):
            n = (value >> shift) & 15
            if self.half is None:
                self.half = n
            else:
                self.out.append(self.half << 4 | n)
                self.half = None

    def align(self):
        if self.half is not None:
            self.out.append(self.half << 4)
            self.half = None


def rle_run(w, length, colour, to_end=False):
    """One run: 1..255 pixels of `colour`, or the rest of the line."""
    if to_end:
        w.put(colour & 3, 4)
        return
    assert 1 <= length <= 255
    code = length << 2 | (colour & 3)
    if length < 4:
        w.put(code, 1)
    elif length < 16:
        w.put(code, 2)
    elif length < 64:
        w.put(code, 3)
    else:
        w.put(code, 4)


def rle_line(w, row, fill_to_end=False):
    """Encode one row of 2-bit indices, then byte-align."""
    x = 0
    n = len(row)
    while x < n:
        c = row[x]
        run = 1
        while x + run < n and row[x + run] == c and run < 255:
            run += 1
        if fill_to_end and x + run == n and run > 1:
            rle_run(w, 0, c, to_end=True)
        else:
            rle_run(w, run, c)
        x += run
    w.align()


def rle_fields(rows, fill_to_end=False):
    """The two interlaced fields: even rows (top) and odd rows (bottom)."""
    top, bottom = Nibbles(), Nibbles()
    for y, row in enumerate(rows):
        rle_line(top if y % 2 == 0 else bottom, row, fill_to_end)
    return bytes(top.out), bytes(bottom.out)


def image(width, height, pattern):
    """Rows of indices from `pattern(x, y)`."""
    return [[pattern(x, y) & 3 for x in range(width)] for y in range(height)]


def glyphs(width, height, seed=0, border=0):
    """A text-like image: index 0 background, 1 body, 2 outline, 3 shadow."""
    def pattern(x, y):
        if x < border or y < border or x >= width - border or y >= height - border:
            return 0
        u = (x * 7 + y * 3 + seed * 5) % 23
        v = (x // 5 + y // 3 + seed) % 7
        if v == 0:
            return 0
        if u < 3:
            return 2
        if u < 5:
            return 3
        return 1
    return image(width, height, pattern)


# ----------------------------------------------------------- control sequences

def cmd_forced():
    return b"\x00"


def cmd_start():
    return b"\x01"


def cmd_stop():
    return b"\x02"


def cmd_palette(e3, e2, e1, e0):
    """SET_COLOR: selectors for pixel values 3, 2, 1, 0 (high nibble first)."""
    return bytes([3, (e3 << 4) | e2, (e1 << 4) | e0])


def cmd_alpha(a3, a2, a1, a0):
    """SET_CONTR: contrast for pixel values 3, 2, 1, 0."""
    return bytes([4, (a3 << 4) | a2, (a1 << 4) | a0])


def cmd_area(x1, x2, y1, y2):
    """SET_DAREA: inclusive columns x1..x2 and rows y1..y2."""
    return bytes([5, x1 >> 4, ((x1 & 15) << 4) | (x2 >> 8), x2 & 255,
                  y1 >> 4, ((y1 & 15) << 4) | (y2 >> 8), y2 & 255])


def cmd_offsets(top, bottom):
    """SET_DSPXA: byte offsets of the top and bottom field pixel data."""
    return b"\x06" + struct.pack(">HH", top, bottom)


def cmd_colcon(body):
    """CHG_COLCON with its parameter area (`body` excludes the size word)."""
    return b"\x07" + struct.pack(">H", len(body) + 2) + body


def cmd_end():
    return b"\xff"


def spu(rows=None, x=0, y=0, sequences=None, pixels=None, fill_to_end=False,
        size_delta=0, control_delta=0, trailing=b""):
    """One SPU.

    `rows` is the image (or `pixels` gives raw `(top, bottom)` field data).
    `sequences` is a list of `(date, [command bytes...])` or
    `(date, [commands], next_override)`; `None` placeholders in commands are
    replaced by the area and field offsets of the image. The last sequence links
    to itself unless overridden.
    """
    if pixels is None:
        top, bottom = rle_fields(rows, fill_to_end)
    else:
        top, bottom = pixels
    height = len(rows) if rows is not None else 2
    width = len(rows[0]) if rows else 2
    data = top + bottom
    off_top, off_bottom = 4, 4 + len(top)
    area = cmd_area(x, x + width - 1, y, y + height - 1)
    offsets = cmd_offsets(off_top, off_bottom)
    if sequences is None:
        sequences = [(0, [cmd_palette(3, 2, 1, 0), cmd_alpha(15, 15, 15, 0), None, None,
                          cmd_start()]), (100, [cmd_stop()])]
    control = 4 + len(data)
    blocks = []
    at = control
    bodies = []
    for seq in sequences:
        commands = []
        placeholders = 0
        for c in seq[1]:
            if c is None:
                commands.append(area if placeholders == 0 else offsets)
                placeholders += 1
            else:
                commands.append(c)
        body = b"".join(commands)
        if not body.endswith(b"\xff") and not (len(seq) > 3 and seq[3]):
            body += cmd_end()
        bodies.append(body)
    starts = []
    for body in bodies:
        starts.append(at)
        at += 4 + len(body)
    for i, (seq, body) in enumerate(zip(sequences, bodies)):
        nxt = starts[i + 1] if i + 1 < len(starts) else starts[i]
        if len(seq) > 2 and seq[2] is not None:
            nxt = seq[2]
        blocks.append(struct.pack(">HH", seq[0], nxt) + body)
    table = b"".join(blocks)
    size = 4 + len(data) + len(table) + size_delta
    return struct.pack(">HH", size & 0xFFFF, (control + control_delta) & 0xFFFF) + data + table + trailing


def simple(width, height, x, y, seed=0, start=0, stop=100, palette=(3, 2, 1, 0),
           alpha=(15, 15, 15, 0), forced=False, border=0, extra=()):
    """A one-picture SPU with start and stop."""
    first = [cmd_palette(*palette), cmd_alpha(*alpha), None, None]
    first.append(cmd_forced() if forced else cmd_start())
    first.extend(extra)
    seqs = [(start, first)]
    if stop is not None:
        seqs.append((stop, [cmd_stop()]))
    return spu(glyphs(width, height, seed, border), x, y, seqs)


# ------------------------------------------------------ extended subpictures

class Bits:
    """A big-endian bit writer."""

    def __init__(self):
        self.out = bytearray()
        self.count = 0

    def put(self, value, width):
        for shift in range(width - 1, -1, -1):
            if self.count % 8 == 0:
                self.out.append(0)
            self.out[-1] |= ((value >> shift) & 1) << (7 - self.count % 8)
            self.count += 1

    def align(self):
        self.count += -self.count % 8


def hd_run(w, length, colour, to_end=False):
    """One 8-bit-mode run: a run flag, a colour width flag and the colour, then
    for runs a length: 2..9 in three bits, 10..136 in seven (0: to the end)."""
    w.put(0 if length == 1 and not to_end else 1, 1)
    if colour < 4:
        w.put(0, 1)
        w.put(colour, 2)
    else:
        w.put(1, 1)
        w.put(colour, 8)
    if length == 1 and not to_end:
        return
    if to_end:
        w.put(1, 1)
        w.put(0, 7)
    elif length <= 9:
        w.put(0, 1)
        w.put(length - 2, 3)
    else:
        assert length <= 136
        w.put(1, 1)
        w.put(length - 9, 7)


def hd_line(w, row, fill_to_end=False):
    x = 0
    while x < len(row):
        c = row[x]
        run = 1
        while x + run < len(row) and row[x + run] == c and run < 136:
            run += 1
        if fill_to_end and x + run == len(row) and run > 1:
            hd_run(w, 0, c, to_end=True)
        else:
            hd_run(w, run, c)
        x += run
    w.align()


def hd_fields(rows, fill_to_end=False):
    top, bottom = Bits(), Bits()
    for y, row in enumerate(rows):
        hd_line(top if y % 2 == 0 else bottom, row, fill_to_end)
    return bytes(top.out), bytes(bottom.out)


def hd_palette(entries):
    """CHG_PAL (0x83): 256 (Y, Cr, Cb) triples."""
    body = bytearray()
    for i in range(256):
        body += bytes(entries[i] if i < len(entries) else (0, 0, 0))
    return b"\x83" + bytes(body)


def hd_contrast(values):
    """0x84: 256 contrast bytes."""
    return b"\x84" + bytes(values[i] if i < len(values) else 0 for i in range(256))


def hd_area(x1, x2, y1, y2):
    return b"\x85" + cmd_area(x1, x2, y1, y2)[1:]


def hd_offsets(top, bottom):
    return b"\x86" + struct.pack(">II", top, bottom)


def hd_spu(rows, x, y, sequences, pixels=None, fill_to_end=False, size_delta=0):
    """An extended unit: a zero size word, 32-bit size and control offset,
    8-bit pixels and sequences with 32-bit links. `None` commands are the
    area then the field offsets of the image."""
    top, bottom = pixels if pixels is not None else hd_fields(rows, fill_to_end)
    width = len(rows[0])
    height = len(rows)
    control = 10 + len(top) + len(bottom)
    area = hd_area(x, x + width - 1, y, y + height - 1)
    offsets = hd_offsets(10, 10 + len(top))
    bodies = []
    for seq in sequences:
        placeholders = 0
        parts = []
        for c in seq[1]:
            if c is None:
                parts.append(area if placeholders == 0 else offsets)
                placeholders += 1
            else:
                parts.append(c)
        body = b"".join(parts)
        if not body.endswith(b"\xff"):
            body += b"\xff"
        bodies.append(body)
    starts, at = [], control
    for body in bodies:
        starts.append(at)
        at += 6 + len(body)
    table = b""
    for i, (seq, body) in enumerate(zip(sequences, bodies)):
        nxt = starts[i + 1] if i + 1 < len(starts) else starts[i]
        if len(seq) > 2 and seq[2] is not None:
            nxt = seq[2]
        table += struct.pack(">HI", seq[0], nxt) + body
    size = 10 + len(top) + len(bottom) + len(table) + size_delta
    return b"\0\0" + struct.pack(">II", size, control) + top + bottom + table


# ------------------------------------------------------------ program streams

def scr_bytes(scr):
    """MPEG-2 pack header SCR (base only), mux rate 10080 kbit/s units."""
    base = scr
    b = bytearray(6)
    b[0] = 0x44 | ((base >> 27) & 0x38) | ((base >> 28) & 0x03)
    b[1] = (base >> 20) & 0xFF
    b[2] = 0x04 | ((base >> 12) & 0xF8) | ((base >> 13) & 0x03)
    b[3] = (base >> 5) & 0xFF
    b[4] = 0x04 | ((base << 3) & 0xF8)
    b[5] = 0x01
    return bytes(b)


def pack_header(scr):
    mux = 25200
    return (b"\x00\x00\x01\xba" + scr_bytes(scr)
            + bytes([(mux >> 14) & 0xFF, (mux >> 6) & 0xFF, ((mux << 2) & 0xFC) | 3, 0xF8]))


def pts_bytes(pts, marker=0x21):
    return bytes([marker | ((pts >> 29) & 0x0E), (pts >> 22) & 0xFF, ((pts >> 14) & 0xFE) | 1,
                  (pts >> 7) & 0xFF, ((pts << 1) & 0xFE) | 1])


def pes_private1(substream, payload, pts=None):
    header = pts_bytes(pts) if pts is not None else b""
    flags = 0x80 if pts is not None else 0x00
    body = bytes([0x81, flags, len(header)]) + header + bytes([substream]) + payload
    return b"\x00\x00\x01\xbd" + struct.pack(">H", len(body)) + body


def padding(length):
    """A padding stream PES of exactly `length` bytes (>= 6)."""
    return b"\x00\x00\x01\xbe" + struct.pack(">H", length - 6) + b"\xff" * (length - 6)


def sectors(substream, data, pts, scr):
    """Sectors carrying one SPU: PTS on the first PES, padded to 2048 bytes."""
    out = []
    first = True
    while data or first:
        head = pack_header(scr)
        overhead = len(head) + 6 + 3 + (5 if first else 0) + 1
        room = SECTOR - overhead
        chunk, data = data[:room], data[room:]
        pes = pes_private1(substream, chunk, pts if first else None)
        sector = head + pes
        gap = SECTOR - len(sector)
        if gap >= 6:
            sector += padding(gap)
        elif gap:
            # Too small for a padding packet: stuff the PES header instead.
            pes = pes_private1(substream, chunk, pts if first else None)
            body = pes[6:]
            stuffed = body[:2] + bytes([body[2] + gap]) + body[3:3 + body[2]] + b"\xff" * gap + body[3 + body[2]:]
            sector = head + b"\x00\x00\x01\xbd" + struct.pack(">H", len(stuffed)) + stuffed
        assert len(sector) == SECTOR, len(sector)
        out.append(sector)
        first = False
        scr += 90
    return out


def raw_sectors(data, pts):
    """Sectors carrying `data` (possibly empty) as one PES chain, PTS first."""
    out = []
    first = True
    while data or first:
        head = pack_header(0)
        room = SECTOR - len(head) - 6 - 3 - (5 if first else 0) - 1
        chunk, data = data[:room], data[room:]
        sector = head + pes_private1(0x20, chunk, pts if first else None)
        gap = SECTOR - len(sector)
        out.append(sector + (padding(gap) if gap else b""))
        first = False
    return out


def program_stream(units, end_code=True):
    """`units` is a list of (substream, spu bytes, pts 90 kHz).

    Returns (stream bytes, [(substream, pts, filepos)]).
    """
    out = bytearray()
    index = []
    scr = 0
    for substream, data, pts in units:
        index.append((substream, pts, len(out)))
        for s in sectors(substream, data, pts, scr):
            out += s
            scr += 90
    if end_code:
        out += b"\x00\x00\x01\xb9"
    return bytes(out), index


# --------------------------------------------------------------------- VobSub

DEFAULT_PALETTE = ["000000", "f0f0f0", "cccccc", "999999", "3333fa", "1111bb", "fa3333",
                   "bb1111", "33fa33", "11bb11", "fafa33", "bbbb11", "fa33fa", "bb11bb",
                   "33fafa", "11bbbb"]


def timestamp(ms):
    sign = "-" if ms < 0 else ""
    ms = abs(ms)
    return "%s%02d:%02d:%02d:%03d" % (sign, ms // 3600000, ms // 60000 % 60, ms // 1000 % 60, ms % 1000)


def idx_text(entries, header=None, size=(720, 480), palette=DEFAULT_PALETTE,
             langs=None, extra_lines=(), custom=None):
    """`entries` maps a track index to a list of (ms or raw text, filepos)."""
    lines = header if header is not None else [
        "# VobSub index file, v7 (do not modify this line!)",
        "#",
        "size: %dx%d" % size if size else None,
        "palette: " + ", ".join(palette) if palette else None,
    ]
    lines = [l for l in lines if l is not None]
    if custom is not None:
        lines.append(custom)
    lines.extend(extra_lines)
    lines.append("")
    lines.append("# Language index in use")
    lines.append("langidx: 0")
    for track in sorted(entries):
        lang = (langs or {}).get(track, "en")
        lines.append("id: %s, index: %d" % (lang, track))
        for item in entries[track]:
            if isinstance(item, str):
                lines.append(item)
                continue
            when, pos = item
            when = timestamp(when) if isinstance(when, int) else when
            lines.append("timestamp: %s, filepos: %09x" % (when, pos))
    return "\n".join(lines) + "\n"


def vobsub(name, out, tracks, ms_from_pts=True, **idx):
    """Write NAME.idx/NAME.sub. `tracks` maps track -> [(spu, pts90k)]."""
    units = []
    for track in sorted(tracks):
        for data, pts in tracks[track]:
            units.append((0x20 + track, data, pts))
    units.sort(key=lambda u: u[2])
    stream, index = program_stream(units)
    entries = {t: [] for t in tracks}
    for substream, pts, pos in index:
        entries[substream - 0x20].append((pts // 90 if ms_from_pts else pts, pos))
    write(out, name + ".sub", stream)
    write(out, name + ".idx", idx_text(entries, **idx).encode())
    return entries


# ------------------------------------------------------------------- VTS IFO

def ifo(palette_ycrcb, pgcs=1, magic=b"DVDVIDEO-VTS", pgci_sector=1, clut_pgc=0):
    """A VTS IFO with a VTS_PGCI table of `pgcs` program chains.

    `palette_ycrcb` is 16 (Y, Cr, Cb) triples for PGC `clut_pgc`; the others
    carry an all-zero table.
    """
    head = bytearray(SECTOR)
    head[0:12] = magic
    struct.pack_into(">I", head, 0x0C, 2)  # last sector of the title set
    struct.pack_into(">I", head, 0x1C, 1)  # last sector of the IFO
    struct.pack_into(">H", head, 0x20, 0x0011)  # version 1.1
    struct.pack_into(">I", head, 0x80, 0x3FF)  # end byte address of VTSI_MAT
    struct.pack_into(">I", head, 0xCC, pgci_sector)  # VTS_PGCI start sector
    pgc_size = 0xEC
    table = bytearray(8 + 8 * pgcs)
    struct.pack_into(">H", table, 0, pgcs)
    offsets = []
    body = bytearray()
    for i in range(pgcs):
        offsets.append(len(table) + len(body))
        pgc = bytearray(pgc_size)
        if i == clut_pgc:
            for j, (yy, cr, cb) in enumerate(palette_ycrcb):
                pgc[0xA4 + 4 * j: 0xA8 + 4 * j] = bytes([0, yy, cr, cb])
        body += pgc
    for i, off in enumerate(offsets):
        struct.pack_into(">II", table, 8 + 8 * i, 0x81000000 | (i + 1) << 24 & 0x7F000000, off)
    pgci = table + body
    struct.pack_into(">I", table, 4, len(pgci) - 1)
    pgci = table + body
    sector = bytearray(pgci) + bytearray(-len(pgci) % SECTOR)
    return bytes(head) + bytes(SECTOR * (pgci_sector - 1)) + bytes(sector)


# ------------------------------------------------------------------- helpers

def write(out, name, data):
    path = os.path.join(out, name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    return path


def sha256(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def mux(out, name, extension):
    """Copy NAME.idx into a Matroska or MP4 file with ffmpeg."""
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        raise SystemExit("ffmpeg is required to mux the Matroska and MP4 copies")
    target = os.path.join(out, name + "." + extension)
    subprocess.run([ffmpeg, "-v", "error", "-y", "-i", os.path.join(out, name + ".idx"),
                    "-map", "0", "-c", "copy", "-fflags", "+bitexact", target], check=True)
    return target


# ------------------------------------------------------------------ Matroska

def ebml_id(value):
    return value.to_bytes((value.bit_length() + 7) // 8, "big")


def ebml_size(n):
    for length in range(1, 9):
        if n < (1 << (7 * length)) - 1:
            return ((1 << (7 * length)) | n).to_bytes(length, "big")
    raise ValueError(n)


def element(ident, body):
    if isinstance(body, int):
        body = body.to_bytes(max(1, (body.bit_length() + 7) // 8), "big")
    return ebml_id(ident) + ebml_size(len(body)) + body


def matroska(codec_private, blocks, scale=1000000, compression=None):
    """A Matroska file with one S_VOBSUB track.

    `blocks` is a list of (timestamp in scale units, data, duration or None).
    `compression` is None, "zlib" (ContentCompAlgo 0, as mkvmerge writes
    VobSub tracks) or a header-stripping prefix (ContentCompAlgo 3).
    """
    header = element(0x1A45DFA3, b"".join([
        element(0x4286, 1), element(0x42F7, 1), element(0x42F2, 4), element(0x42F3, 8),
        element(0x4282, b"matroska"), element(0x4287, 4), element(0x4285, 2)]))
    info = element(0x1549A966, element(0x2AD7B1, scale) + element(0x4D80, b"dvd_streams")
                   + element(0x5741, b"dvd_streams"))
    entry = [element(0xD7, 1), element(0x73C5, 1), element(0x83, 0x11),
             element(0x86, b"S_VOBSUB")]
    if codec_private is not None:
        entry.append(element(0x63A2, codec_private))
    if compression is not None:
        algo = element(0x4254, 0) if compression == "zlib" else element(0x4254, 3) + element(0x4255, compression)
        encoding = element(0x6240, element(0x5031, 0) + element(0x5032, 1) + element(0x5033, 0)
                           + element(0x5034, algo))
        entry.append(element(0x6D80, encoding))
    tracks = element(0x1654AE6B, element(0xAE, b"".join(entry)))
    clusters = []
    for when, data, duration in blocks:
        if compression == "zlib":
            data = zlib.compress(data, 9)
        elif compression is not None:
            assert data.startswith(compression)
            data = data[len(compression):]
        block = element(0xA1, b"\x81" + struct.pack(">h", 0) + b"\x80" + data)
        group = block + (element(0x9B, duration) if duration is not None else b"")
        clusters.append(element(0x1F43B675, element(0xE7, when) + element(0xA0, group)))
    segment = element(0x18538067, info + tracks + b"".join(clusters))
    return header + segment


# --------------------------------------------------------------------- cases

def pattern_rows(width, height, seed):
    """Deterministic pseudo-random 2-bit rows (a linear congruential sequence)."""
    state = seed * 2654435761 % (1 << 32) or 1
    rows = []
    for _ in range(height):
        row = []
        for _ in range(width):
            state = (state * 1103515245 + 12345) % (1 << 31)
            row.append((state >> 16) & 3)
        rows.append(row)
    return rows


def std(alpha=(15, 15, 15, 0), palette=(3, 2, 1, 0)):
    """Palette, contrast, area and offsets placeholders."""
    return [cmd_palette(*palette), cmd_alpha(*alpha), None, None]


def field_data(lines):
    """Raw field bytes from lines of (count, colour) runs; count 0 fills."""
    w = Nibbles()
    for line in lines:
        for count, colour in line:
            rle_run(w, count, colour, to_end=count == 0)
        w.align()
    return bytes(w.out)


def raw_unit(data, sequences, prefix=b""):
    """A unit from raw pixel `data` and sequences whose offsets the caller
    states; `sequences` are (date, body bytes, next or None)."""
    control = 4 + len(prefix) + len(data)
    starts, at = [], control
    for date, body, _ in sequences:
        starts.append(at)
        at += 4 + len(body)
    table = b""
    for i, (date, body, nxt) in enumerate(sequences):
        link = nxt if nxt is not None else (starts[i + 1] if i + 1 < len(starts) else starts[i])
        table += struct.pack(">HH", date, link) + body
    return struct.pack(">HH", 4 + len(prefix) + len(data) + len(table), control) + prefix + data + table


def chain(units, step=90000, start=90000):
    """(unit, pts) pairs one second apart."""
    return [(u, start + i * step) for i, u in enumerate(units)]


def commands_units():
    """Every control command, several sequences, colour and contrast changes."""
    g = glyphs(40, 12, 3, border=2)
    top, bottom = rle_fields(g)
    area = cmd_area(100, 139, 200, 211)
    offsets = cmd_offsets(4, 4 + len(top))
    p0, a0 = cmd_palette(3, 2, 1, 0), cmd_alpha(15, 15, 15, 0)
    units = [
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (40, [cmd_palette(7, 6, 5, 4), None, None]),
                          (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (40, [cmd_alpha(8, 4, 15, 0), area, offsets]),
                          (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, [area, offsets, p0, a0, cmd_start()]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, [p0, a0, area, cmd_start()]), (10, [offsets]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_colcon(b"\x0f\xff\xff\xff"), cmd_palette(1, 1, 1, 1), cmd_start()]),
                          (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, [p0, a0, cmd_colcon(b"\x00\x00\x00\x00\x0f\xff\xff\xff"), area, offsets,
                               cmd_start()]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [b"\x08", cmd_start()]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (100, [cmd_stop()]), (150, [cmd_start()]),
                          (300, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (100, [cmd_area(120, 139, 210, 221), offsets]),
                          (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_area(10, 49, 20, 31), cmd_start()])]),
        spu(g, 100, 200, [(0, std() + [cmd_offsets(4, 4), cmd_start()]), (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start(), cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (100, [cmd_area(139, 100, 200, 211), offsets]),
                          (200, [cmd_stop()])]),
        spu(g, 100, 200, [(0, std() + [cmd_start()]), (100, [cmd_alpha(0, 0, 0, 0), area, offsets]),
                          (200, [cmd_stop()])]),
    ]
    return units


def timing_units():
    g = glyphs(24, 8, 5, border=1)
    s = lambda seqs: spu(g, 300, 400, seqs)
    return [
        s([(50, std() + [cmd_start()]), (150, [cmd_stop()])]),
        s([(0, std() + [cmd_start()])]),
        s([(100, std() + [cmd_start()]), (50, [cmd_stop()])]),
        s([(10, [cmd_stop()]), (100, std() + [cmd_start()])]),
        s([(0, std())]),
        s([(0, std()), (100, [cmd_stop()])]),
        s([(0xFFFF, std() + [cmd_start()]), (0xFFFF, [cmd_stop()])]),
        s([(1, std() + [cmd_start()]), (2, [cmd_stop()])]),
        s([(0, std() + [cmd_start()]), (100, [cmd_stop()]), (200, [cmd_stop()])]),
    ]


def link_units():
    g = glyphs(24, 8, 6, border=1)
    base = spu(g, 30, 40, [(0, std() + [cmd_start()]), (100, [cmd_stop()]), (200, [cmd_stop()])])
    control = struct.unpack(">H", base[2:4])[0]
    first = control + 4 + 3 + 3 + 7 + 5 + 1 + 1
    second = first + 6
    out = []
    for target in [None, second, first + 1, control, control + 2, len(base) - 5, len(base) - 4, 0x7000, 4]:
        out.append(spu(g, 30, 40, [(0, std() + [cmd_start()], target), (100, [cmd_stop()]), (200, [cmd_stop()])]))
    # backward link from the second sequence, and a link beyond the unit
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start()]), (100, [cmd_stop()], control)]))
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start()]), (100, [cmd_stop()], 0xFFF0)]))
    # truncated operands, a missing end marker, an unterminated final sequence
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start()]), (100, [cmd_stop(), cmd_palette(1, 1, 1, 1)[:2]], None, True)]))
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start()]), (100, [cmd_stop()], None, True)]))
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start(), cmd_alpha(15, 15, 15, 0)[:2]], None, True)]))
    out.append(spu(g, 30, 40, [(0, std() + [cmd_start(), cmd_area(1, 2, 3, 4)[:5]], None, True)]))
    return out


def pixel_units():
    """Field offsets, odd sizes, run edge cases and crops."""
    out = []
    for w, h, x, y in [(17, 7, 11, 13), (1, 3, 0, 0), (3, 2, 5, 5), (720, 2, 0, 0), (2, 575, 718, 1),
                       (255, 9, 3, 4), (64, 64, 100, 100), (16, 1, 10, 10), (513, 3, 7, 9)]:
        rows = pattern_rows(w, h, w * 31 + h)
        out.append(spu(rows, x, y, [(0, std((15, 15, 15, 15)) + [cmd_start()]), (100, [cmd_stop()])]))
    g = glyphs(16, 6, 1)
    full = field_data([[(16, 1)]] * 3)
    two = field_data([[(16, 1)]] * 2)
    other = field_data([[(16, 2)]] * 3)
    area = cmd_area(10, 25, 10, 15)
    body = lambda top, bottom: cmd_palette(3, 2, 1, 0) + cmd_alpha(15, 15, 15, 0) + area + cmd_offsets(top, bottom) + cmd_start() + cmd_end()
    out.append(raw_unit(two, [(0, body(4, 4), None)]))
    out.append(raw_unit(full, [(0, body(4, 4), None)]))
    out.append(raw_unit(other + full, [(0, body(4 + len(other), 4), None)]))
    out.append(raw_unit(full, [(0, body(4, 4 + len(full)), None)]))
    out.append(raw_unit(full + full, [(0, body(4, 0x7000), None)]))
    out.append(raw_unit(full + full, [(0, body(0, 4 + len(full)), None)]))
    # bottom field after the control table, complete, short and cut mid-code
    for tail in [other, field_data([[(16, 2)]] * 2), field_data([[(16, 2)]] * 2) + b"\x22",
                 field_data([[(16, 2)]] * 2) + b"\x01", field_data([[(16, 2)]] * 2) + b"\x0a"]:
        control = 4 + len(full)
        seq = struct.pack(">HH", 0, control) + body(4, control + 4 + 3 + 3 + 7 + 5 + 1 + 1)
        unit = full + seq + tail
        out.append(struct.pack(">HH", 4 + len(unit), control) + unit)
    # runs: exact, overflowing, short lines, fill codes, every code length
    for lines in [[[(17, 1)]] + [[(16, 1)]] * 2, [[(10, 1), (10, 2)]] + [[(16, 1)]] * 2,
                  [[(10, 1)]] + [[(16, 1)]] * 2, [[(4, 2), (0, 1)]] + [[(0, 3)]] * 2,
                  [[(1, 1), (3, 2), (4, 3), (8, 1)], [(15, 2), (1, 3)], [(16, 1)]]]:
        data = field_data(lines)
        out.append(raw_unit(data + full, [(0, body(4, 4 + len(data)), None)]))
    wide = field_data([[(255, 1), (255, 2), (255, 3), (0, 1)]] * 2)
    out.append(raw_unit(wide, [(0, cmd_palette(3, 2, 1, 0) + cmd_alpha(15, 15, 15, 15) + cmd_area(0, 1023, 0, 3)
                                + cmd_offsets(4, 4) + cmd_start() + cmd_end(), None)]))
    # transparent borders, fully transparent, fully opaque, mixed contrast, menu
    g = glyphs(30, 10, 2, border=3)
    for alpha in [(15, 15, 15, 0), (0, 0, 0, 0), (15, 15, 15, 15), (1, 7, 0, 3), (0, 15, 0, 0), (2, 0, 0, 0)]:
        out.append(spu(g, 50, 60, [(0, std(alpha) + [cmd_start()]), (100, [cmd_stop()])]))
    out.append(spu(g, 50, 60, [(0, std() + [cmd_forced()]), (100, [cmd_stop()])]))
    out.append(spu(g, 50, 60, [(0, std((0, 0, 0, 0)) + [cmd_forced()]), (100, [cmd_stop()])]))
    out.append(spu(g, 50, 60, [(0, std() + [cmd_start()]), (50, [cmd_forced()]), (100, [cmd_stop()])]))
    out.append(spu(g, 50, 60, [(0, [cmd_palette(3, 2, 1, 0), cmd_alpha(15, 15, 15, 0), cmd_forced()]),
                               (50, [None, None]), (100, [cmd_stop()])]))
    return out


def forced_units():
    g = glyphs(32, 10, 7, border=2)
    out = []
    for i in range(6):
        cmd = cmd_forced() if i % 2 else cmd_start()
        out.append(spu(g, 40 + i, 50, [(0, std() + [cmd]), (120, [cmd_stop()])]))
    return out


def guess_units(count=48):
    """Random selector and contrast words for the automatic palette."""
    g = pattern_rows(12, 4, 99)
    out = []
    state = 7
    for i in range(count):
        state = (state * 1103515245 + 12345) % (1 << 31)
        sel = [(state >> s) & 15 for s in (0, 4, 8, 12)]
        alpha = [((state >> s) & 15) if (state >> (s + 1)) & 1 else 0 for s in (16, 20, 24, 27)]
        if i % 7 == 0:
            sel = [sel[0]] * 4
        out.append(spu(g, 10, 10, [(0, [cmd_palette(*sel), cmd_alpha(*alpha), None, None, cmd_start()]),
                                   (100, [cmd_stop()])]))
    return out


def basic_units(count=12):
    out = []
    for i in range(count):
        w, h = 120 + 37 * (i % 5), 24 + 6 * (i % 3)
        out.append(spu(glyphs(w, h, i, border=2), 300 - w // 2, 380 + (i % 2) * 40,
                       [(0, std((15, 15, 15, 0), (i % 16, 2, 1, 0)) + [cmd_start()]), (100 + 40 * i, [cmd_stop()])]))
    return out


def img8(width, height, pattern):
    return [[pattern(x, y) & 255 for x in range(width)] for y in range(height)]


def ramp_palette(seed):
    """256 (Y, Cr, Cb) colours."""
    return [(16 + (i * 7 + seed) % 220, 128 + (i * 3 + seed) % 33 - 16, 128 - (i * 5 + seed) % 29 + 14)
            for i in range(256)]


def hd_units():
    """Eight-bit subpictures: colours, contrast, runs of every form, crops."""
    pal = ramp_palette(0)
    opaque = [0] * 256
    keyed = [255] + [0] * 255
    graded = [(i * 37) % 256 for i in range(256)]
    show = lambda contrast=opaque, palette=pal: [hd_palette(palette), hd_contrast(contrast), None, None]
    stop = (120, [cmd_stop()])
    out = []
    shapes = [
        img8(16, 6, lambda x, y: (x * 3 + y * 7) % 256),
        img8(40, 12, lambda x, y: 0 if x < 3 or y < 2 or x > 35 else (x // 4 * 17 + y) % 256 or 1),
        img8(300, 4, lambda x, y: [17, 2, 1, 200][min(3, x // 90)]),
        img8(33, 9, lambda x, y: (x * x + y * 3) % 7 * 40),
        img8(1, 5, lambda x, y: y * 50),
        img8(720, 2, lambda x, y: x % 256),
    ]
    for i, rows in enumerate(shapes):
        contrast = [opaque, keyed, graded][i % 3]
        out.append(hd_spu(rows, 10 + i, 20 + i, [(0, show(contrast) + [cmd_start()]), stop],
                          fill_to_end=i % 2 == 1))
    g = shapes[0]
    t, b = hd_fields(g)
    area, offsets = hd_area(50, 65, 60, 65), hd_offsets(10, 10 + len(t))
    out += [
        hd_spu(g, 50, 60, [(0, show() + [cmd_alpha(15, 8, 1, 0), cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [cmd_alpha(15, 8, 1, 0)] + show() + [cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), area, offsets, hd_contrast(opaque), cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), area, offsets, cmd_start()]), (10, [hd_contrast(opaque)]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal)]), (10, [hd_contrast(opaque), area, offsets, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, show() + [hd_palette(pal[::-1]), cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, show() + [cmd_start()]), (10, [hd_palette(pal[::-1]), area, offsets]), stop]),
        hd_spu(g, 50, 60, [(0, show() + [cmd_start()]), (10, [hd_palette(pal[::-1])]), stop]),
        hd_spu(g, 50, 60, [(0, show(keyed) + [cmd_forced()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), hd_contrast(opaque), cmd_palette(1, 2, 3, 4), None, None, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_contrast(opaque), None, None, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [None, None, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), hd_contrast(opaque), area]), (10, [cmd_area(50, 65, 60, 65), offsets, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), hd_contrast(opaque), cmd_area(50, 65, 60, 65), offsets, cmd_start()]), stop]),
        hd_spu(g, 50, 60, [(0, [hd_palette(pal), hd_contrast(opaque), area, cmd_offsets(10, 10 + len(t)), cmd_start()]), stop]),
    ]
    # four-colour pixels in an extended unit, and extended commands in a traditional one
    two = glyphs(16, 6, 3)
    t2, b2 = rle_fields(two)
    out.append(hd_spu(two, 0, 0, [(0, [cmd_palette(3, 2, 1, 0), cmd_alpha(15, 15, 15, 0), cmd_area(5, 20, 5, 10),
                                       hd_offsets(10, 10 + len(t2)), cmd_start()]), stop], pixels=(t2, b2)))
    out.append(spu(g, 0, 0, [(0, [hd_palette(pal), hd_contrast(opaque), hd_area(30, 45, 40, 45), cmd_offsets(4, 4 + len(t)),
                                  cmd_start()])], pixels=(t, b)))
    return out


def hd_pixel_units():
    """Eight-bit run edge cases and the end of the unit."""
    pal = ramp_palette(5)
    keyed = [255] + [0] * 255
    def lines(spec):
        w = Bits()
        for line in spec:
            for count, colour in line:
                hd_run(w, count, colour, to_end=count == 0)
            w.align()
        return bytes(w.out)
    full = lines([[(16, 5)]] * 3)
    body = hd_palette(pal) + hd_contrast(keyed) + hd_area(10, 25, 20, 25)
    def unit(top, bottom, tail=b"", bottom_after=False):
        control = 10 + len(top) + (0 if bottom_after else len(bottom))
        seq_len = 6 + len(body) + 9 + 2
        off = control + seq_len if bottom_after else 10 + len(top)
        seq = struct.pack(">HI", 0, control) + body + hd_offsets(10, off) + b"\x01\xff"
        data = top + (b"" if bottom_after else bottom) + seq + (bottom if bottom_after else b"") + tail
        return b"\0\0" + struct.pack(">II", len(data) + 10, control) + data
    out = [unit(full, full)]
    for spec in [[[(17, 5)]] + [[(16, 5)]] * 2, [[(10, 5)]] + [[(16, 5)]] * 2, [[(4, 200), (0, 7)]] + [[(0, 9)]] * 2,
                 [[(16, 5)]] * 2, [[(1, 3), (1, 250)] * 8] * 3, [[(9, 4), (7, 1)], [(2, 9), (14, 8)], [(0, 255)]]]:
        out.append(unit(lines(spec), full))
    two = lines([[(16, 6)]] * 2)
    for tail in [b"", b"\0", b"\0\0", b"\0" * 4, b"\0" * 8]:
        out.append(unit(full, two, tail=tail))
    straddle = lines([[(15, 6), (1, 200)]] * 3)
    for drop in [0, 1, 2]:
        out.append(unit(full, straddle[:len(straddle) - drop], bottom_after=True))
    for zeros in [24, 23, 30]:
        out.append(unit(full, b"\0" * zeros, bottom_after=True))
    return out


def carry_units():
    """Colour selectors and contrast carry over between units."""
    g = glyphs(16, 6, 1)
    h = img8(16, 6, lambda x, y: (x * 3 + y * 7) % 256)
    pal = ramp_palette(9)
    stop = (100, [cmd_stop()])
    return [
        spu(g, 10, 10, [(0, [cmd_palette(7, 6, 5, 4), cmd_alpha(15, 15, 15, 0), None, None, cmd_start()]), stop]),
        spu(g, 30, 30, [(0, [None, None, cmd_start()]), stop]),
        spu(g, 30, 30, [(0, [cmd_palette(3, 2, 1, 0), None, None, cmd_start()]), stop]),
        spu(g, 10, 10, [(0, [cmd_palette(1, 2, 3, 4), cmd_alpha(15, 15, 15, 0), cmd_area(10, 12, 10, 15), None,
                             cmd_start()]), (50, [cmd_palette(9, 9, 9, 8)])]),
        spu(g, 30, 30, [(0, [None, None, cmd_start()]), stop]),
        spu(g, 10, 10, [(0, [cmd_alpha(0, 0, 0, 0), None, None, cmd_start()]), (40, [cmd_alpha(15, 8, 4, 0)])]),
        spu(g, 30, 30, [(0, [None, None, cmd_start()]), stop]),
        hd_spu(h, 10, 20, [(0, [hd_palette(pal), hd_contrast([0] * 256), None, None, cmd_start()]), stop]),
        spu(g, 30, 30, [(0, [None, None, cmd_start()]), stop]),
        hd_spu(h, 30, 20, [(0, [hd_palette(pal[::-1]), None, None, cmd_start()]), stop]),
        hd_spu(h, 30, 20, [(0, [hd_contrast([0] * 256), None, None, cmd_start()]), stop]),
        spu(g, 30, 30, [(0, [cmd_alpha(15, 8, 1, 0), None, None, cmd_start()]), stop]),
        hd_spu(h, 30, 20, [(0, [hd_palette(pal), None, None, cmd_start()]), stop]),
    ]


def hd_reassembly_scenarios():
    pal = ramp_palette(3)
    u = hd_spu(img8(24, 8, lambda x, y: (x + 5 * y) % 256), 10, 20,
               [(0, [hd_palette(pal), hd_contrast([0] * 256), None, None, cmd_start()]), (100, [cmd_stop()])])
    b = spu(glyphs(20, 4, 2), 50, 60)
    control = struct.unpack(">I", u[6:10])[0]
    big = u[:2] + struct.pack(">I", len(u) + 50) + u[6:]
    late = struct.pack(">HI", 0, len(u) + 64) + u[6:]
    return [
        ("split-early", [u[:500], u[500:], b]),
        ("split-before-sequence", [big[:control + 5], b, u]),
        ("split-at-sequence", [big[:control + 6], b]),
        ("fragments", [u[:9], u[:10], u[10:200], u[200:], b]),
        ("control-beyond-size", [u[:6] + struct.pack(">I", len(u) + 100) + u[10:500], b]),
        ("declared-larger", [big, b]),
        ("late-control", [late[:300], b]),
    ]


def sectors_of(substream, data, pts):
    return sectors(substream, data, pts, 0)


def idx_file(out, name, entries, sub, **idx):
    write(out, name + ".sub", sub)
    write(out, name + ".idx", idx_text(entries, **idx).encode())
    return name + ".idx"


def raw_idx(out, name, lines, sub):
    write(out, name + ".sub", sub)
    write(out, name + ".idx", ("\n".join(lines) + "\n").encode())
    return name + ".idx"


def fixture_packets(path):
    """The packets of an authored test fixture: a `.spu` is one unit; a
    `SMDVC002` `.bin` holds configuration text and a packet sequence."""
    with open(path, "rb") as f:
        data = f.read()
    if not path.endswith(".bin"):
        return None, [data]
    assert data[:8] == b"SMDVC002", path
    private = struct.unpack(">I", data[8:12])[0]
    configuration = data[12:12 + private]
    at = 12 + private
    count = struct.unpack(">I", data[at:at + 4])[0]
    at += 12
    packets = []
    for _ in range(count):
        n = struct.unpack(">I", data[at + 9:at + 13])[0]
        at += 13
        packets.append(data[at:at + n])
        at += n
    return configuration, packets


FIXTURE_SETS = [
    "harness/subtitles/dvd-fixtures/hd-source-fixtures",
    "harness/subtitles/dvd-fixtures/hd-additive-fixtures",
    "harness/subtitles/dvd-fixtures/hd-policy-fixtures",
    "harness/subtitles/dvd-fixtures/hd-multi-fixtures",
    "harness/subtitles/dvd-fixtures/hd-context-fixtures",
]


def generate(out):
    """Write every input; return the cases as (id, file, arguments, companions)."""
    cases = []

    def case(cid, file, *arguments, companions=()):
        cases.append((cid, file, ["dvd", *arguments], list(companions)))

    def single(name, units, **idx):
        vobsub(name, out, {0: chain(units)}, **idx)
        return name + ".idx"

    case("vobsub-basic", single("basic", basic_units()))
    case("vobsub-commands", single("commands", commands_units()))
    case("vobsub-timing", single("timing", timing_units()))
    case("vobsub-links", single("links", link_units()))
    case("vobsub-pixels", single("pixels", pixel_units()))
    case("vobsub-guess-palette", single("guess", guess_units(), palette=None))
    case("vobsub-guess-palette-pixels", single("guess-pixels", pixel_units(), palette=None))
    case("vobsub-forced", single("forced", forced_units()))
    case("vobsub-forced-only", "forced.idx", "opt:forced_subs_only=1")
    case("vobsub-forced-only-off", "forced.idx", "opt:forced_subs_only=0")
    case("vobsub-palette-option", "basic.idx", "opt:palette=112233,445566,778899,aabbcc,0xddeeff,AbCdEf, 102030 ,1122334455")
    case("vobsub-palette-option-short", "basic.idx", "opt:palette=ff0000,00ff00")
    case("vobsub-palette-option-invalid", "basic.idx", "opt:palette=zz,445566")
    case("vobsub-palette-option-empty", "basic.idx", "opt:palette=")

    # IFO colour tables
    ycrcb = [(16 + 14 * i, 128 + 7 * (i - 8), 240 - 13 * i) for i in range(16)]
    write(out, "VTS_01_0.IFO", ifo(ycrcb))
    write(out, "VTS_02_0.IFO", ifo([(i * 17, 255 - i * 17, (i * 53) % 256) for i in range(16)], pgcs=3, pgci_sector=2))
    write(out, "VTS_03_0.IFO", ifo(ycrcb, magic=b"DVDVIDEO-VMG"))
    write(out, "VTS_04_0.IFO", ifo(ycrcb)[:SECTOR + 8 + 8 + 0xA4 + 20])
    write(out, "VTS_05_0.IFO", ifo(ycrcb, pgcs=2, clut_pgc=1))
    base = ".research/subtitle-corpus/dvd/"
    for n, label in [(1, "vts"), (2, "three-pgcs"), (3, "wrong-magic"), (4, "truncated"), (5, "second-pgc")]:
        ifo_name = "VTS_%02d_0.IFO" % n
        case("vobsub-ifo-palette-" + label, "basic.idx", "opt:ifo_palette=" + base + ifo_name,
             companions=[ifo_name])
    case("vobsub-ifo-palette-missing", "basic.idx", "opt:ifo_palette=" + base + "VTS_99_0.IFO")
    case("vobsub-ifo-and-palette-option", "basic.idx", "opt:ifo_palette=" + base + "VTS_01_0.IFO",
         "opt:palette=010203,040506", companions=["VTS_01_0.IFO"])
    case("vobsub-guess-ifo-palette", "guess.idx", "opt:ifo_palette=" + base + "VTS_01_0.IFO",
         companions=["VTS_01_0.IFO"])

    # reassembly: fragments, short and invalid starts, excess bytes, units sharing a packet
    a = spu(glyphs(16, 6, 1, border=1), 10, 10)
    b = spu(glyphs(20, 4, 2), 50, 60)
    c = spu(glyphs(12, 8, 3), 30, 40)
    control = struct.unpack(">H", a[2:4])[0]
    big = struct.pack(">H", len(a) + 20) + a[2:]
    # Units with a control offset of 0, whose colour commands follow the
    # header, then units that show the colours carried over.
    bare = spu(glyphs(16, 6, 1, border=1), 30, 30, [(0, [None, None, cmd_start()]), (100, [cmd_stop()])])
    narrow_zero = cmd_palette(7, 6, 5, 4) + cmd_alpha(15, 8, 4, 0) + cmd_end()
    narrow_zero = struct.pack(">HH", 4 + len(narrow_zero) + 4, 0) + narrow_zero + b"\0" * 4
    wide_zero = cmd_palette(1, 2, 3, 4) + cmd_alpha(15, 15, 8, 0) + cmd_end()
    wide_zero = struct.pack(">HII", 0, 10 + len(wide_zero) + 4, 0) + wide_zero + b"\0" * 4
    scenarios = [
        ("split", [a[:30], a[30:], b, a[:10], a[10:], c, a[:20], a[20:21], a[21:25], a[25:]]),
        ("short-start", [a[:1], b, a[:9], a[9:], c]),
        ("short-start-garbage", [a[:1], a[1:], b]),
        ("invalid-control", [a[:2] + struct.pack(">H", 3) + b"\0" * 6, a[:30], a[30:],
                             a[:2] + struct.pack(">H", 0xFFFF) + b"\0" * 6, a,
                             a[:2] + struct.pack(">H", 6) + b"\0" * 6, b]),
        ("valid-control-garbage", [a[:2] + struct.pack(">H", 30) + b"\0" * 6, a, b]),
        ("excess", [a[:30], a[30:] + b, a + b, c]),
        ("new-unit-while-pending", [a[:30], b, c]),
        ("empty-while-pending", [a[:30], b"", a[30:], b"", b]),
        ("declared-size", [struct.pack(">H", len(a) - 5) + a[2:], b"\0\3" + a[2:], b"\0\0" + a[2:], b]),
        ("control-offset", [a[:2] + struct.pack(">H", len(a) + 10) + a[4:],
                            a[:2] + struct.pack(">H", 2) + a[4:], c]),
        ("control-offset-zero", [a, narrow_zero, bare, wide_zero, bare]),
        ("control-header-complete", [big, big[:control + 4], b, big[:control + 3], a, b,
                                     big[:20], big[20:], b"\0" * 20, c]),
        ("long-tail", [a[:20], a[20:] + b"\0" * 3000, b]),
    ]
    for name, packets in scenarios:
        stream = bytearray()
        entries = []
        for i, data in enumerate(packets):
            entries.append((1000 + 100 * i, len(stream)))
            for sector in raw_sectors(data, (1000 + 100 * i) * 90):
                stream += sector
        stream += b"\0\0\1\xb9"
        case("vobsub-reassembly-" + name, idx_file(out, "reassembly-" + name, {0: entries}, bytes(stream)))

    case("vobsub-hd", single("hd", hd_units()))
    case("vobsub-hd-guess-palette", single("hd-guess", hd_units(), palette=None))
    case("vobsub-hd-pixels", single("hd-pixels", hd_pixel_units()))
    case("vobsub-colour-carry", single("colour-carry", carry_units()))
    case("vobsub-colour-carry-guess", single("colour-carry-guess", carry_units(), palette=None))
    for name, packets in hd_reassembly_scenarios():
        stream = bytearray()
        entries = []
        for i, data in enumerate(packets):
            entries.append((1000 + 100 * i, len(stream)))
            for sector in raw_sectors(data, (1000 + 100 * i) * 90):
                stream += sector
        stream += b"\0\0\1\xb9"
        case("vobsub-hd-reassembly-" + name, idx_file(out, "hd-reassembly-" + name, {0: entries}, bytes(stream)))

    # the unit fixtures of the crate's tests, each in its own VobSub file
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
    for directory in FIXTURE_SETS:
        set_name = os.path.basename(directory).replace("-fixtures", "")
        for name in sorted(os.listdir(os.path.join(root, directory))):
            if not name.endswith((".spu", ".bin")):
                continue
            configuration, packets = fixture_packets(os.path.join(root, directory, name))
            stream = bytearray()
            entries = []
            for i, data in enumerate(packets):
                entries.append((1000 + 100 * i, len(stream)))
                for sector in raw_sectors(data, (1000 + 100 * i) * 90):
                    stream += sector
            stream += b"\0\0\1\xb9"
            stem = "fixture-%s-%s" % (set_name, name.rsplit(".", 1)[0])
            header = None
            if configuration is not None:
                header = [line for line in configuration.decode("latin-1").split("\n") if line]
            file = idx_file(out, "fixtures/" + stem, {0: entries}, bytes(stream),
                            **({"header": header} if header is not None else {}))
            case("vobsub-" + stem, file)

    # demuxer: extents, ordering, multiple tracks, misaligned positions, foreign packets
    big2 = spu(pattern_rows(300, 60, 3), 10, 10)
    s2 = spu(glyphs(20, 10, 1), 100, 200)
    s3 = spu(glyphs(30, 12, 2), 50, 300)
    ps, index = program_stream([(0x20, big2, 90000), (0x20, s2, 180000), (0x20, s3, 270000)])
    p1, p2 = index[1][2], index[2][2]
    s2_end = p1 + 14 + 6 + 3 + 5 + 1 + len(s2)
    for name, entries in [
            ("extent-continuation", [(1000, 0), (1500, 0x800), (2000, p1), (3000, p2)]),
            ("extent-unlisted", [(1000, 0), (3000, p2)]),
            ("extent-unsorted", [(3000, p2), (1000, 0), (2000, p1)]),
            ("extent-ties", [(1000, p2), (1000, p1), (900, 0), (3000, p1), (2000, p1)]),
            ("extent-duplicate", [(1000, p1), (2000, p1), (2500, p2)]),
            ("extent-misaligned", [(1000, 0), (2000, 0x808), (2500, s2_end + 8), (3000, p2), (4000, p2 + 0x7f0)]),
            ("extent-boundaries", [(1000, 0), (1100, 0x7ff), (1200, 0x80e), (2000, p1), (2500, s2_end),
                                   (2600, s2_end + 1), (3000, p2)]),
            ("extent-pes-end", [(1000, 0), (2000, p1), (2500, s2_end - 1), (3000, p2)])]:
        case("vobsub-" + name, idx_file(out, name, {0: entries}, ps))
    a1 = spu(glyphs(20, 10, 4), 100, 200)
    a2 = spu(glyphs(40, 8, 5), 10, 20)
    ps2, _ = program_stream([(0x20, big2, 90000), (0x21, a1, 95000), (0x20, s2, 180000), (0x21, a2, 190000)])
    two = {0: [(1000, 0), (2000, 0x2800)], 1: [(1055, 0x2000), (2111, 0x3000)]}
    for stream_n in (0, 1):
        case("vobsub-two-tracks-%d" % stream_n, idx_file(out, "two-tracks", two, ps2), "stream=%d" % stream_n)
    spanning = {0: [(1000, 0), (2000, 0x3000)], 1: [(1055, 0x2000)]}
    case("vobsub-track-spanning", idx_file(out, "track-spanning", spanning, ps2))
    secs = sectors(0x20, big2, 90000, 0)
    other = sectors(0x21, a1, 95000, 0)
    def mid(sector):
        return b"".join(secs[:2]) + sector + b"".join(secs[2:]) + b"\0\0\1\xb9"
    def pes(code, payload, header=bytes([0x81, 0, 0])):
        return b"\0\0\1" + bytes([code]) + struct.pack(">H", len(header) + len(payload)) + header + payload
    def sector(*parts):
        s = pack_header(0) + b"".join(parts)
        return s + padding(SECTOR - len(s))
    def after(sector):
        return b"".join(secs) + sector + b"".join(sectors(0x20, s2, 180000, 0)) + b"\0\0\1\xb9"
    for name, payload in [("trailing-subpicture", b"".join(other)),
                          ("trailing-video", sector(pes(0xE0, b"\x11" * 100))),
                          ("trailing-audio", sector(pes(0xBD, b"\x80\x01\x00\x01" + b"\x22" * 50,
                                                        bytes([0x81, 0x80, 5]) + pts_bytes(5000)))),
                          ("trailing-navigation", sector(pes(0xBF, b"\x33" * 100, b"")))]:
        nxt = len(b"".join(secs)) + len(payload)
        case("vobsub-" + name, idx_file(out, name, {0: [(1000, 0), (2000, nxt)]}, after(payload)))
    for name, payload in [("foreign-subpicture", b"".join(other)),
                          ("foreign-video", sector(pes(0xE0, b"\x11" * 100))),
                          ("foreign-audio", sector(pes(0xC0, b"\x22" * 50, bytes([0x81, 0x80, 5]) + pts_bytes(5000)))),
                          ("foreign-navigation", sector(pes(0xBF, b"\x33" * 100, b""))),
                          ("foreign-padding", pack_header(0) + padding(SECTOR - 14))]:
        case("vobsub-" + name, idx_file(out, name, {0: [(1000, 0)], 1: [(1055, 0x1000)]}, mid(payload)))

    # index text variants
    unit = spu(glyphs(16, 6, 1), 10, 10)
    sub = b"".join(sectors(0x20, unit, 90000, 0)) + b"".join(sectors(0x21, unit, 90000, 0))
    T = lambda ms, pos=0: "timestamp: %s, filepos: %09x" % (timestamp(ms), pos)
    P = "palette: " + ", ".join(DEFAULT_PALETTE)
    variants = [
        ("idx-header-lines", ["# c", "size: 720x576", "org: 0, 0", "Palette: 112233", "weird line",
                              "  palette: 445566", "langidx: 0", "delay: 00:00:01:000", "id: en, index: 0",
                              P, T(1000), "# trailing comment"], ()),
        ("idx-crlf", ["size: 720x576\r", "palette: 010203, 040506\r", "", "", "id: en, index: 0\r", T(1000) + "\r"], ()),
        ("idx-no-header", ["id: en, index: 0", T(1000)], ()),
        ("idx-delays", ["id: en, index: 0", "delay: 00:00:01:000", T(1000), "delay: -00:00:00:500", T(2000),
                        "delay: 1:2:3:4", T(3000)], ()),
        ("idx-timestamp-forms", ["id: en, index: 0", "timestamp: 00:00:01:000, filepos: 0",
                                 "timestamp: 1:2:3:4, filepos: 0", "timestamp:00:00:05:000,filepos:000000000",
                                 "timestamp: 00:00:06:000, filepos: 000000800"], ()),
        ("idx-track-order", ["delay: 00:00:01:000", "id: de, index: 1", T(2000, 0x800), "id: en, index: 0", T(1000)],
         ("stream=0", "stream=1")),
        ("idx-empty-track", ["id: en, index: 0", "id: de, index: 1", T(2000, 0x800)], ()),
        ("idx-repeated-track", ["id: en, index: 0", T(1000), "id: en, index: 0", T(3000)], ()),
        ("idx-no-index", ["id: en", T(1000)], ()),
        ("idx-alt", ["id: en, index: 0, alt: English", T(1000)], ()),
        ("idx-past-end", ["id: en, index: 0", T(1000), T(2000, 0x10000)], ()),
        ("idx-palette-lines", ["palette: 111111, 222222, 333333, 444444", "palette: 555555, 666666",
                               "id: en, index: 0", T(1000)], ()),
        ("idx-palette-malformed", ["palette: zz", "id: en, index: 0", T(1000)], ()),
        ("idx-palette-empty", ["palette:", "id: en, index: 0", T(1000)], ()),
        ("idx-custom-colours", [P, "custom colors: ON, tridx: 1010, colors: 111111, 222222, 333333, 444444",
                                "id: en, index: 0", T(1000)], ()),
        ("idx-custom-colours-only", ["custom colors: ON, tridx: 0110, colors: 111111, 222222, 333333, 444444",
                                     "id: en, index: 0", T(1000)], ()),
    ]
    for name, lines, streams in variants:
        file = raw_idx(out, name, lines, sub)
        for arguments in (streams or ((),)):
            arguments = (arguments,) if isinstance(arguments, str) else arguments
            suffix = "-" + arguments[0].replace("=", "") if arguments else ""
            case("vobsub-" + name + suffix, file, *arguments)

    # program streams: video, navigation and audio packets, two subpicture streams, the parser
    def vob_sector(*parts):
        s = pack_header(0) + b"".join(parts)
        return s + (padding(SECTOR - len(s)) if SECTOR - len(s) >= 6 else b"")
    def sp(data, pts=None, sub=0x20):
        return vob_sector(pes_private1(sub, data, pts))
    nav = vob_sector(pes(0xBF, b"\0" * 980, b""), pes(0xBF, b"\1" * 1018, b""))
    video = vob_sector(pes(0xE0, b"\0\0\1\xb3" + b"\x10" * 1000, bytes([0x81, 0xC0, 10]) + pts_bytes(90000, 0x31) + pts_bytes(87000, 0x11)))
    audio = vob_sector(pes(0xBD, b"\x80\x01\x00\x01" + b"\x0b\x77" + b"\x00" * 500, bytes([0x81, 0x80, 5]) + pts_bytes(90000)))
    track1 = sectors(0x20, big2, 180000, 0)
    track2 = sectors(0x21, s3, 185000, 0)
    vob = b"".join([nav, video, audio] + track1[:2] + [video] + track2 + track1[2:] + [nav]
                   + sectors(0x20, s2, 270000, 0) + sectors(0x21, b, 280000, 0)) + b"\0\0\1\xb9"
    write(out, "titles.vob", vob)
    case("vob-titles-0", "titles.vob", "stream=0")
    case("vob-titles-1", "titles.vob", "stream=1")
    A, B, C = a, b, c
    parser = [sp(A[:30], 90000), sp(A[30:], 95000), sp(B, 180000),
              sp(A[:30], 190000), sp(A[30:]), sp(B),
              sp(A + B, 200000), sp(C, 210000),
              sp(A[:30], 220000), sp(A[30:] + B[:20], 225000), sp(B[20:], 230000), sp(C, 240000), sp(C, 245000),
              sp(A, None), sp(B, 250000), sp(A[:1], 260000), sp(A[1:]),
              sp(b"", 270000), sp(A, 280000), sp(A, 290000), sp(A, 290000),
              sp(B[:20], 300000), sp(A, 310000), sp(B, 320000), sp(C[:40], 330000)]
    write(out, "parser.vob", b"".join(parser) + b"\0\0\1\xb9")
    case("vob-parser", "parser.vob")
    chunks = [big2[i:i + 300] for i in range(0, len(big2), 300)]
    split = [sp(ch, 400000 + 100 * i) for i, ch in enumerate(chunks)] + [sp(B, 500000)]
    write(out, "split.vob", b"".join(split) + b"\0\0\1\xb9")
    case("vob-split", "split.vob")
    write(out, "menu.sub", b"".join(b"".join(sectors(0x20, u, 90000 * (i + 1), 0))
                                    for i, u in enumerate(pixel_units()[-4:])))
    case("vob-bare-sub", "menu.sub")

    # Matroska: CodecPrivate variants and block durations
    g = glyphs(16, 6, 1, border=1)
    cp = ("size: 720x480\npalette: " + ", ".join(DEFAULT_PALETTE) + "\n").encode()
    blocks = [
        (1000, spu(g, 10, 10, [(0, std() + [cmd_start()])]), 2500),
        (5000, spu(g, 10, 10, [(0, std() + [cmd_start()]), (0, [cmd_stop()])]), 2500),
        (9000, spu(g, 10, 10, [(50, std() + [cmd_start()])]), 1234),
        (12000, spu(g, 10, 10, [(0, std() + [cmd_start()]), (90, [cmd_stop()])]), 4321),
        (15000, spu(g, 10, 10, [(0, std() + [cmd_start()])]), None),
        (18000, a[:30], 100), (18100, a[30:], 777),
    ]
    write(out, "durations.mkv", matroska(cp, blocks))
    case("mkv-durations", "durations.mkv")
    write(out, "durations-scale.mkv", matroska(cp, blocks, scale=100000))
    case("mkv-durations-scale", "durations-scale.mkv")
    for name, private in [("no-private", None), ("palette-twice", b"palette: 111111\npalette: 445566, 778899\n"),
                          ("palette-nospace", b"palette:112233,445566,778899,aabbcc\n"),
                          ("palette-upper", b"Palette: 112233\n"),
                          ("custom-colours", cp + b"custom colors: ON, tridx: 0000, colors: 111111, 222222, 333333, 444444\n")]:
        write(out, "private-%s.mkv" % name, matroska(private, [(t * 1000, u, None) for t, u in
                                                               enumerate(guess_units(8) + basic_units(4))]))
        case("mkv-private-" + name, "private-%s.mkv" % name)

    units = basic_units(6) + pixel_units()[:6]
    write(out, "zlib.mkv", matroska(cp, [(t * 1000, u, 900) for t, u in enumerate(units)], compression="zlib"))
    case("mkv-zlib", "zlib.mkv")
    stripped = [u for u in units if u.startswith(units[0][:1])]
    write(out, "header-stripping.mkv", matroska(cp, [(t * 1000, u, None) for t, u in enumerate(stripped)],
                                                compression=units[0][:1]))
    case("mkv-header-stripping", "header-stripping.mkv")

    hd = hd_units()
    hd_vob = b"".join(b"".join(sectors(0x20, u, 90000 * (i + 1), 0)) for i, u in enumerate(hd)) + b"\0\0\1\xb9"
    write(out, "hd.vob", hd_vob)
    case("vob-hd", "hd.vob")
    write(out, "hd.mkv", matroska(cp, [(t * 1000, u, 800) for t, u in enumerate(hd + carry_units())]))
    case("mkv-hd", "hd.mkv")

    # ffmpeg-muxed copies of VobSub inputs
    for name in ["basic", "commands", "pixels"]:
        mux(out, name, "mkv")
        case("mkv-muxed-" + name, name + ".mkv")
        target = os.path.join(out, name + ".mp4")
        subprocess.run([shutil.which("ffmpeg"), "-v", "error", "-y", "-i", os.path.join(out, name + ".idx"),
                        "-map", "0", "-c", "copy", "-use_editlist", "0", "-fflags", "+bitexact", target], check=True)
        case("mp4-muxed-" + name, name + ".mp4")
    edited = os.path.join(out, "edited.mp4")
    vobsub("edited", out, {0: chain(basic_units(5), start=900000)})
    subprocess.run([shutil.which("ffmpeg"), "-v", "error", "-y", "-i", os.path.join(out, "edited.idx"),
                    "-map", "0", "-c", "copy", "-fflags", "+bitexact", edited], check=True)
    case("mp4-edit-list", "edited.mp4")
    return cases


FATE = [
    ("fate-vobsub", "fate/sub/vobsub.idx", [], ["fate/sub/vobsub.sub"], "sub/vobsub.idx"),
    ("fate-242-4-0", "fate/filter/242_4.mkv", ["stream=0"], [], "filter/242_4.mkv"),
    ("fate-242-4-1", "fate/filter/242_4.mkv", ["stream=1"], [], "filter/242_4.mkv"),
    ("fate-242-4-2", "fate/filter/242_4.mkv", ["stream=2"], [], "filter/242_4.mkv"),
    ("fate-dvd-single-frame-0", "fate/mpeg2/dvd_single_frame.vob", ["stream=0"], [], "mpeg2/dvd_single_frame.vob"),
    ("fate-dvd-single-frame-1", "fate/mpeg2/dvd_single_frame.vob", ["stream=1"], [], "mpeg2/dvd_single_frame.vob"),
    ("fate-dvd-still-frame-0", "fate/mpeg2/dvd_still_frame.vob", ["stream=0"], [], "mpeg2/dvd_still_frame.vob"),
    ("fate-dvd-still-frame-1", "fate/mpeg2/dvd_still_frame.vob", ["stream=1"], [], "mpeg2/dvd_still_frame.vob"),
]


def fold(reference, path, arguments, root):
    run = subprocess.run([reference, "sub-fold", path, *arguments], capture_output=True, text=True, cwd=root)
    if run.returncode != 0:
        return {"error": run.stderr.strip()}
    result = json.loads(run.stdout.splitlines()[-1])
    return {k: result[k] for k in ("packets", "bytes", "digest", "host_dependent") if k in result}


def host_target():
    """The target triple the reference was built for: the host's."""
    run = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True)
    return next(line.split(": ", 1)[1] for line in run.stdout.splitlines() if line.startswith("host: "))


def main():
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
    corpus = os.path.join(root, ".research", "subtitle-corpus")
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join(corpus, "dvd")
    os.makedirs(out, exist_ok=True)
    cases = generate(out)
    reference = os.path.join(root, "target", "harness", "ffmpeg-subtitles-worker")
    fate = os.path.join(root, ".research", "fate-samples")
    rows = []
    for cid, file, arguments, companions in cases:
        rows.append((cid, "dvd/" + file, arguments, ["dvd/" + c for c in companions] +
                     (["dvd/" + file[:-4] + ".sub"] if file.endswith(".idx") else []),
                     "authored-synthetic", "harness/subtitles/dvd_streams.py"))
    for cid, file, arguments, companions, source in FATE:
        for name in [file] + companions:
            target = os.path.join(corpus, name)
            os.makedirs(os.path.dirname(target), exist_ok=True)
            shutil.copyfile(os.path.join(fate, name[len("fate/"):]), target)
        rows.append((cid, file, ["dvd", *arguments], companions, "fetched-not-redistributed",
                     "rsync://fate-suite.ffmpeg.org/fate-suite/" + source))
    # A case the reference flags host-dependent goes to subtitle-dvd-host,
    # pinned for this target beside the pins other targets recorded.
    features = os.path.join(root, "harness", "features")
    host_path = os.path.join(features, "subtitle-dvd-host", "corpus.json")
    with open(host_path) as f:
        pinned = {case["id"]: case["expected_by_target"] for case in json.load(f)["cases"]}
    manifest, host = {"schema": 1, "cases": []}, {"schema": 1, "cases": []}
    for cid, file, arguments, companions, rights, source in rows:
        path = os.path.join(corpus, file)
        entry = {"id": cid, "profile": cid, "file": file, "sha256": sha256(path), "rights": rights,
                 "source": source, "arguments": arguments}
        if companions:
            entry["companions"] = [{"file": c, "sha256": sha256(os.path.join(corpus, c))} for c in companions]
        if os.path.exists(reference):
            expected = fold(reference, path, arguments, root)
            if expected.get("host_dependent"):
                by_target = {**pinned.get(cid, {}), host_target(): expected}
                entry["expected_by_target"] = dict(sorted(by_target.items()))
                host["cases"].append(entry)
                continue
            entry["expected"] = expected
        manifest["cases"].append(entry)
    for target, document in [(os.path.join(features, "subtitle-dvd", "corpus.json"), manifest), (host_path, host)]:
        with open(target, "w") as f:
            json.dump(document, f, indent=2)
            f.write("\n")
    print("%d cases, %d pinned per target" % (len(manifest["cases"]), len(host["cases"])))


if __name__ == "__main__":
    main()
