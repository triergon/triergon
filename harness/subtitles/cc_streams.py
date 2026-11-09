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


# --------------------------------------------------------------- encodes


def run(args):
    subprocess.run(args, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)


def encode(codec, frames_count, path, open_gop=False):
    """The base video: a small test pattern at 29.97 Hz with B pictures; MPEG-2
    as an elementary stream, H.264 and HEVC in MPEG-TS. `open_gop` makes the
    H.264 and HEVC keyframes after the first non-IDR I pictures (with recovery
    point SEI) and CRA pictures."""
    source = ["-f", "lavfi", "-i", "testsrc=size=64x48:rate=30000/1001"]
    common = [FFMPEG, "-v", "error", "-y", "-fflags", "+bitexact"] + source + [
        "-frames:v", str(frames_count), "-flags", "+bitexact", "-threads", "1"]
    if codec == "mpeg2":
        run(common + ["-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-b:v", "200k", "-f", "mpeg2video", path])
    elif codec == "h264":
        run(common + ["-c:v", "libx264", "-preset", "veryfast",
                      "-x264-params", "aud=1:keyint=12:min-keyint=12:scenecut=0:threads=1:bframes=2:b-adapt=0:b-pyramid=none"
                      + (":open-gop=1" if open_gop else ""),
                      "-f", "mpegts", path])
    else:
        run(common + ["-c:v", "libx265", "-preset", "veryfast",
                      "-x265-params", "aud=1:bframes=2:b-adapt=0:b-pyramid=0:keyint=12:min-keyint=12:scenecut=0:pools=1:frame-threads=1:log-level=error"
                      + (":open-gop=1" if open_gop else ""),
                      "-f", "mpegts", path])


def remux(source, path, raw_format=None):
    """`source` in another container, packets copied. A raw elementary stream
    (`raw_format`) gets generated timestamps at 29.97 Hz."""
    container = {".ts": "mpegts", ".mkv": "matroska", ".mp4": "mp4"}[os.path.splitext(path)[1]]
    raw = ["-fflags", "+bitexact+genpts", "-r", "30000/1001", "-f", raw_format] if raw_format else ["-fflags", "+bitexact"]
    run([FFMPEG, "-v", "error", "-y"] + raw + ["-i", source, "-c", "copy", "-flags", "+bitexact",
         "-f", container, path])


def ts_rewrite_video(ts, transform):
    """`ts` with each video PES payload n replaced by transform(n, payload), or
    dropped when it returns None; the PES headers (PTS, DTS) and the other
    packets stay. The rewritten PES has
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
            # of bitstream last in the one before the third: decoding starts
            # again at both CRA pictures, and FFmpeg drops the pictures still
            # waiting for output there (no_output_of_prior_pics_flag).
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
    return cases


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
                expected = {k: record[k] for k in ("packets", "bytes", "digest")}
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
