#!/usr/bin/env python3
"""Author the subtitle runners' inputs and record their expected folds.

    subtitle_corpus.py capsules TX3G_SAMPLES
        Write the subtitle-text inputs under harness/fixtures/subtitle-text/:
        session capsules from repository fixtures (the captured mov_text cases,
        the samples the tx3g semantic tests build, recorded with
        TRIERGON_TX3G_SAMPLES=TX3G_SAMPLES, see
        crates/subtitles/tests/text_tx3g_semantics.rs, the recorded charset
        receiver cases and the ASS packets of the text fixtures), and the
        mov_text sessions, ASS packets and SubRip and WebVTT documents
        authored here.

    subtitle_corpus.py record FEATURE REFERENCE
        Write harness/features/FEATURE/corpus.json (subtitle-text or
        subtitle-pgs-capsules), each case's expected fold recorded by running the
        reference worker REFERENCE on it.

    subtitle_corpus.py record subtitle-text-host REFERENCE TARGET
        Record each subtitle-text-host case's fold for target triple TARGET with
        the reference worker REFERENCE built for it, keeping the other targets'.

Run from the repository root. Every input is authored-synthetic and already in
the repository or derived byte for byte from repository fixtures.
"""
import hashlib
import json
import re
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path.cwd()
OUT = ROOT / "harness/fixtures/subtitle-text"
MOV_TEXT = ROOT / "crates/subtitles/tests/native-mov-text-fixtures"
TEXT = ROOT / "crates/subtitles/tests/text-fixtures/fixtures"
CHARSET = ROOT / "crates/subtitles/tests/receiver-mapping-data/cases74.rs"
PGS = ROOT / "crates/subtitles/tests/native-compatibility-gold"
# The packet clock of every authored ASS packet: 1/1000, PTS 1 s, duration 2 s.
ASS_CLOCK = (1, 1000, 1000, 2000)
# The clock the tx3g semantic tests give every sample: 1/90000, PTS 0, 1.5 s.
TX3G_CLOCK = (1, 90000, 0, 135000)


# ASS packets authored here, each a capsule with the clock of the others: the
# decode call's UTF-8 check, which stops at the first NUL and refuses U+FFFE.
ASS_PACKETS = {
    "utf8-after-nul": b"0,0,Default,,0,0,0,,A\x00\xff",
    "nul-only": b"\x00",
    "overlong": b"0,0,Default,,0,0,0,,\xc0\x80",
    "surrogate": b"0,0,Default,,0,0,0,,\xed\xa0\x80",
    "above-unicode": b"0,0,Default,,0,0,0,,\xf4\x90\x80\x80",
    "largest-scalar": b"0,0,Default,,0,0,0,,\xf4\x8f\xbf\xbf",
    "noncharacter-fffe": b"0,0,Default,,0,0,0,,\xef\xbf\xbe",
    "noncharacter-ffff": b"0,0,Default,,0,0,0,,\xef\xbf\xbf",
    "truncated": b"0,0,Default,,0,0,0,,\xe6\xb0",
}

# Labelled ASS packets for the single-byte charsets the decode call's
# sub_charenc accepts: the reference converts 0x80 to 0x9F to the C1 controls
# under both Latin-1 labels and refuses the bytes the Windows code pages leave
# undefined.
LINE = b"0,0,Default,,0,0,0,,"
SINGLE_BYTE_PACKETS = {
    "windows-1251-text": (b"windows-1251", LINE + bytes([0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]) + bytes(range(0x80, 0x98)) + bytes(range(0x99, 0x100))),
    "windows-1251-undefined": (b"windows-1251", LINE + b"A\x98"),
    "windows-1252-text": (b"windows-1252", LINE + bytes(b for b in range(0x80, 0x100) if b not in (0x81, 0x8D, 0x8F, 0x90, 0x9D))),
    "windows-1252-undefined": (b"windows-1252", LINE + b"A\x81"),
    "latin1-c1-controls": (b"latin1", LINE + bytes(range(0x80, 0x100))),
    "iso-8859-1-c1-controls": (b"iso-8859-1", LINE + bytes(range(0x80, 0x100))),
}


# mov_text sessions authored here for the override tags the decoder writes
# where a style record (TS 26.245 StyleRecord) or the highlight starts or ends:
# a sample description (TextSampleEntry body) or None, then samples.
def tx3g_box(kind, body):
    return struct.pack(">I", len(body) + 8) + kind + body


def tx3g_style(start, end, font=1, face=0, size=16, rgb=(255, 255, 255), alpha=255):
    return struct.pack(">HHHBB", start, end, font, face, size) + bytes(rgb) + bytes([alpha])


def tx3g_styl(*records):
    return tx3g_box(b"styl", struct.pack(">H", len(records)) + b"".join(records))


def tx3g_hlit(start, end):
    return tx3g_box(b"hlit", struct.pack(">HH", start, end))


def tx3g_hclr(r, g, b, a=255):
    return tx3g_box(b"hclr", bytes([r, g, b, a]))


def tx3g_sample(text, *boxes):
    text = text.encode() if isinstance(text, str) else text
    return struct.pack(">H", len(text)) + text + b"".join(boxes)


def tx3g_description(face=0, rgb=(255, 255, 255), alpha=255, fonts=((1, b"Arial"),)):
    """Display flags, justification, background, text box, a default style of
    font 1 at size 16, and the font table."""
    head = struct.pack(">IbbBBBB", 0, 0, -1, 0, 0, 0, 255) + bytes(8)
    table = struct.pack(">H", len(fonts)) + b"".join(struct.pack(">HB", i, len(n)) + n for i, n in fonts)
    return head + tx3g_style(0, 0, 1, face, 16, rgb, alpha) + tx3g_box(b"ftab", table)


def tx3g_sessions():
    s, st, hl, hc = tx3g_sample, tx3g_style, tx3g_hlit, tx3g_hclr

    def one(text, *records, extra=()):
        return s(text, tx3g_styl(*records), *extra)

    plain = tx3g_description()
    red, green = (0x11, 0x22, 0x33), (0x44, 0x55, 0x66)
    fonts = tx3g_description(fonts=(
        (1, b"Arial"), (2, b"Two"), (3, b""), (2, b"Deux"), (4, b"Sp ace,{x}"), (5, b"X\0Y"),
        (6, b"\xff\xfe"), (7, b"A\xef\xbf\xbe"), (8, b"A\xef\xbf\xbf"), (9, b"A\xc3"),
    ))
    long_names = tx3g_description(fonts=((1, b"Arial"),) + tuple((7, bytes([0x41 + i]) * 200) for i in range(24)))
    return {
        # Face flags written where they differ from the default's, set or clear.
        "face-against-bold": (tx3g_description(face=1), [
            one("abcdef", st(1, 3, face=f)) for f in (0, 2, 4, 6, 7)
        ] + [one("abcdef", st(1, 3, face=1, size=20))]),
        "face-against-all": (tx3g_description(face=7), [one("abcdef", st(1, 3, face=f)) for f in (0, 3, 5)]),
        # Size, colour and alpha: decimal, BGR without leading zeros, 255 - alpha in two digits.
        "size-colour-alpha": (plain, [
            one("abcdef", st(1, 3, size=0)), one("abcdef", st(1, 3, size=255)),
            one("abcdef", st(1, 3, rgb=(0x0A, 0, 0))), one("abcdef", st(1, 3, rgb=(0, 0, 0x0A))),
            one("abcdef", st(1, 3, alpha=0xFE)), one("abcdef", st(1, 3, alpha=0xF0)),
            one("abcdef", st(1, 3, rgb=red, alpha=0x80)),
            one("abcdef", st(1, 3, font=2, face=7, size=30, rgb=red, alpha=0x10)),
        ]),
        "colour-against-alpha": (tx3g_description(rgb=red, alpha=0x40), [
            one("abcdef", st(1, 3, rgb=red, alpha=0x41)), one("abcdef", st(1, 3, rgb=(0x11, 0x22, 0x34), alpha=0x40)),
        ]),
        # Without a sample description the default style is all zeros.
        "no-description": (None, [one("abcdef", st(1, 3, font=3, face=1)), one("abcdef", st(1, 3, font=0, size=0, rgb=(0, 0, 0), alpha=0))]),
        # Every entry with the style's font ID, in table order; none for a
        # missing ID; a name up to its NUL; a name that is not UTF-8 or holds
        # U+FFFE refuses the sample and still takes a read order.
        "font-table": (fonts, [
            one("abcdef", st(1, 3, font=f)) for f in (2, 3, 4, 5, 10, 0)
        ] + [one("abcdef", st(1, 3, font=1, face=1)), one("abcdef", st(1, 3, font=6)), s("abcdef"),
             one("abcdef", st(1, 3, font=7)), one("abcdef", st(1, 3, font=8)), one("abcdef", st(1, 3, font=9)),
             one("abcdef", st(1, 3, font=2))]),
        "font-long-names": (long_names, [one("abcdef", st(1, 3, font=7))]),
        # Record ranges: beyond the text, gaps, adjacent records, records equal
        # to the default or empty, unsorted and overlapping records.
        "ranges": (plain, [
            one("abcd", st(2, 9, face=1)), one("abcd", st(4, 6, face=1)), one("abcd", st(0, 4, face=1)),
            one("abcd", st(0, 1, face=1), st(2, 3, face=2)),
            one("abcd", st(0, 1, face=1), st(1, 2, face=2), st(2, 3, face=4)),
            one("abcd", st(0, 1, face=1), st(1, 2), st(2, 3, face=1)),
            one("abcd", st(0, 1, face=1), st(1, 1, face=2), st(1, 2, face=1)),
            one("abcd", st(2, 3, face=1), st(0, 1, face=2)),
            one("abcd", st(0, 3), st(1, 2, face=1)), one("abcd", st(2, 2, face=2), st(1, 3, face=1)),
            one("abcd", st(0, 2, face=1), st(2, 4, face=1, size=20)),
            one("abcd", st(1, 3, font=9)), one("a\nb\r\nc", st(1, 4, face=1)),
            one("Å水z\U0001f600q", st(1, 3, face=1)),
        ]),
        # The transitions at a NUL that ends the visible text are written;
        # those at the end of the text and after the NUL are not.
        "nul-transitions": (fonts, [
            one(b"ab\0cd", st(0, 2, face=1)), one(b"ab\0cd", st(2, 3, face=1)), one(b"ab\0cd", st(2, 3, font=2)),
            s(b"ab\0cd", hl(2, 2)), s(b"ab\0cd", hl(1, 2)), one(b"ab\0cd", st(3, 4, face=1)),
            one(b"ab\0cd", st(3, 4, font=6)), one(b"ab\0", st(0, 2, face=1)), one(b"\0ab", st(0, 2, face=1)),
            one(b"ab\0\xffd", st(2, 3, face=1)), one(b"ab", st(0, 2, face=1), extra=[hl(2, 2)]),
            one(b"ab", st(2, 3, font=6)), one(b"ab\0cd", st(0, 2, face=1), st(2, 5, face=2)),
            one(b"ab\0cd", st(2, 3, font=6)),
        ]),
        # A highlight end restores the colour of the style in effect.
        "highlight-style-colour": (plain, [
            one("abcd", st(0, 4, rgb=red), extra=[hl(1, 2)]),
            one("abcd", st(0, 4, rgb=red), extra=[hl(1, 2), hc(0xAA, 0xBB, 0xCC)]),
            one("abcd", st(0, 4, alpha=0x20), extra=[hl(1, 2)]),
            one("abcd", st(0, 2, rgb=red), extra=[hl(1, 3)]),
            one("abcd", st(0, 2, rgb=red), extra=[hl(1, 3), hc(0xAA, 0xBB, 0xCC)]),
            one("abcd", st(2, 5, rgb=red), extra=[hl(1, 3)]),
            one("abcd", st(2, 5, rgb=red), extra=[hl(1, 3), hc(0xAA, 0xBB, 0xCC)]),
            one("abcd", st(0, 2, rgb=red), st(2, 4, rgb=green), extra=[hl(1, 3)]),
            one("abcd", st(0, 2, rgb=red), st(2, 4, rgb=green), extra=[hl(1, 3), hc(1, 2, 3)]),
            one("abcd", st(0, 2, rgb=red), st(2, 4, rgb=green), extra=[hl(1, 2)]),
            one("abcd", st(0, 2, rgb=red), st(2, 4, rgb=green), extra=[hl(2, 3)]),
            one("abcd", st(0, 4, rgb=red), extra=[hl(1, 2), hc(0xFF, 0xFF, 0xFF)]),
            s("abcd", hl(1, 2)),
        ]),
        # At one character: the style end, the style start, then the
        # highlight start and end.
        "highlight-coincident": (plain, [
            one("abcd", st(1, 3, face=1, rgb=red), extra=[hl(1, 3)]),
            one("abcd", st(1, 3, face=1, rgb=red), extra=[hl(1, 3), hc(0xAA, 0xBB, 0xCC)]),
            one("abcd", st(0, 2, rgb=red), extra=[hl(2, 4)]),
            one("abcd", st(2, 4, rgb=red), extra=[hl(0, 2)]),
            one("abcd", st(2, 4, rgb=red), extra=[hl(0, 2), hc(0xAA, 0xBB, 0xCC)]),
            one("abcd", st(1, 2, face=1), extra=[hl(1, 1)]), one("abcd", st(1, 2, face=1), extra=[hl(2, 2)]),
            s("abcd", hl(0, 0)), one("abcd", st(1, 2, face=1), extra=[hl(0, 3)]),
        ]),
        # Inverted, partly and wholly outside, the last box, colour box first.
        "highlight-ranges": (plain, [
            s("abcd", hl(3, 1)), s("abcd", hl(2, 9)), s("abcd", hl(5, 6)), s("abcd", hl(4, 4)),
            s("abcd", hl(0, 1), hl(2, 3)), s("abcd", hc(0xAA, 0xBB, 0xCC, 0x80), hl(1, 3)),
        ]),
        "highlight-coloured-default": (tx3g_description(rgb=red, alpha=0x40), [
            s("abcd", hl(1, 2)), s("abcd", hl(1, 2), hc(1, 2, 3)),
            one("abcd", st(0, 4, rgb=green, alpha=0x40), extra=[hl(1, 2)]),
        ]),
    }


T1 = "00:00:01,000 --> 00:00:02,000"
T2 = "00:00:03,000 --> 00:00:04,000"
V1 = "00:01.000 --> 00:02.000"
V2 = "00:03.000 --> 00:04.000"


def utf16be(text):
    return b"\xfe\xff" + text.encode("utf-16-be")


def utf16le(text):
    return b"\xff\xfe" + text.encode("utf-16-le")


# Documents authored here, one per rule of the srt and webvtt demuxers that
# the documents of the text fixtures do not reach.
DOCUMENTS = {
    "srt-sorted": f"1\n00:00:05,000 --> 00:00:06,000\nB\n\n2\n{T1}\nA\n\n",
    "srt-equal-start-order": f"1\n00:00:01,000 --> 00:00:03,000\nA\n\n2\n{T1}\nB\n\n",
    "srt-lines-before-first-cue": f"garbage\n1\n{T1}\nA\n\n",
    "srt-text-across-blank-lines": f"1\n{T1}\nA\n\n\n\nB\n\ngarbage\n2\n{T2}\nC\n\n",
    "srt-number-without-blank-line": f"1\n{T1}\nA\n2\n{T2}\nB\n\n",
    "srt-number-blank-then-timing": f"1\n{T1}\nA\n\n2\n\n{T2}\nB\n\n",
    "srt-number-prefix": f"1\n{T1}\nA\n3a\n{T2}\nB\n",
    "srt-negative-number-is-text": f"1\n{T1}\nA\n-3\n{T2}\nB\n",
    "srt-number-not-last-is-text": f"1\n{T1}\nA\n42\nB\n\n",
    "srt-only-line-whole-number": f"1\n{T1}\n100\n{T2}\nB\n",
    "srt-only-line-number-prefix": f"1\n{T1}\n100:5\n{T2}\nB\n",
    "srt-empty-cue-dropped": f"1\n{T1}\n1\n{T2}\nB\n\n",
    "srt-coordinates": f"1\n{T1}  X1:10 X2:20 Y1:5 Y2:6\nA\n\n",
    "srt-short-fields": "1\n0:0:1,5 --> 0:0:2,25\nA\n\n",
    "srt-unbounded-fields": "1\n00:61:01,000 --> 00:00:02,000\nA\n\n",
    "srt-wide-fraction": "1\n00:00:01,1234 --> 00:00:02,000\nA\n\n",
    "srt-dot-separator": "1\n00:00:01.000 --> 00:00:02.000\nA\n\n",
    "srt-no-space-around-arrow": "1\n  00:00:01,000-->00:00:02,000\nA\n\n",
    "srt-signed-fields": "1\n-00:00:01,000 --> +00:00:02,000\nA\n\n",
    "srt-no-fraction": "1\n00:00:01 --> 00:00:02\nA\n\n",
    "srt-single-dash": "1\n00:00:01,000 -> 00:00:02,000\nA\n\n",
    "srt-cr-lines": f"1\r{T1}\rA\r\r",
    "srt-no-final-line-end": f"1\n{T1}\nA",
    "srt-nul-ends-document": f"1\n{T1}\nA\n\n2\n{T2}\nB\x00C\n\n",
    "srt-nul-in-only-line": f"1\n{T1}\nA\x00B\n\n",
    "srt-duration-wraps": "1\n00:00:00,000 --> 999:00:00,000\nA\n\n",
    "srt-negative-duration-until-next": f"1\n00:00:05,000 --> 00:00:04,000\nA\n\n2\n00:00:07,000 --> 00:00:08,000\nB\n\n3\n00:00:06,000 --> 00:00:06,500\nC\n\n",
    "srt-negative-duration-last": "1\n00:00:10,000 --> 00:00:04,000\nA\n\n2\n00:00:05,000 --> 00:00:06,000\nB\n\n",
    "srt-zero-duration-kept": f"1\n00:00:05,000 --> 00:00:05,000\nA\n\n2\n{T2}\nB\n\n",
    "srt-repeat-dropped": f"1\n{T1}\nA\n\n2\n00:00:05,000 --> 00:00:06,000\nB\n\n3\n{T1}\nA\n\n",
    "srt-repeat-apart-kept": f"1\n{T1}\nA\n\n2\n{T1}\nB\n\n3\n{T1}\nA\n\n",
    "srt-repeat-after-completion": "1\n00:00:05,000 --> 00:00:04,000\nA\n\n2\n00:00:05,000 --> 00:00:04,000\nA\n\n3\n00:00:07,000 --> 00:00:08,000\nC\n\n",
    "srt-utf16-line-start-cut": utf16be(f"1\n{T1}\n\u00c5x\n\u00c6y\n\n"),
    "srt-utf16-crlf-no-cut": utf16be(f"1\r\n{T1}\r\n\u00c5x\r\n\u00c6y\r\n\r\n"),
    "srt-utf16-cr-cut": utf16le(f"1\r{T1}\r\u6c34x\r\u00c6y\r\r"),
    "srt-utf16-blank-line-pairs": utf16be(f"1\n{T1}\nA\n\n\u00c5\n\n\n\u00c6\r\n\n\u00c7\n\n"),
    "srt-utf16-surrogate-ends": utf16be(f"1\n{T1}\nA\nB") + b"\xdc\x00\x00C",
    "vtt-sorted": f"WEBVTT\n\n00:05.000 --> 00:06.000\nB\n\n{V1}\nA\n\n",
    "vtt-header-without-blank-line": f"WEBVTT\n{V1}\nA\n\n",
    "vtt-no-header": f"{V1}\nA\n\n{V2}\nB\n\n",
    "vtt-two-identifier-lines": f"WEBVTT\n\na\nb\n{V1}\nA\n\n",
    "vtt-skipped-block-prefixes": f"WEBVTT\n\nNOTEX\n\nSTYLES\n\nWEBVTT\n{V1}\nA\n\n{V2}\nB\n\n",
    "vtt-byte-order-mark-not-skipped": f"\ufeff{V1}\nA\n\n",
    "vtt-short-fraction": "WEBVTT\n\n00:01.12 --> 00:02.000\nA\n\n",
    "vtt-three-digit-hours": "WEBVTT\n\n001:00:01.000 --> 001:00:02.000\nA\n\n",
    "vtt-comma-fraction": "WEBVTT\n\n00:01,000 --> 00:02,000\nA\n\n",
    "vtt-arrow-on-later-line": f"WEBVTT\n\nid\n00:01.000 x\ny --> 00:02.000 s\nA\n\n",
    "vtt-end-on-next-line": "WEBVTT\n\n00:01.000 -->\n00:02.000 s\nA\n\n",
    "vtt-settings-after-end-word": "WEBVTT\n\n00:01.000 --> 00:02.000x  s\nA\n\n00:03.000 --> 99:+1. 7\nB\n\n",
    "vtt-settings-end-at-cr": "WEBVTT\n\n00:01.000 --> 00:02.000 a\rb\nA\n\n",
    "vtt-timing-in-payload": f"WEBVTT\n\n{V1}\nA\n{V2}\nB\n\n",
    "vtt-empty-payload": f"WEBVTT\n\n{V1}\n\n{V2}\nB\n\n",
    "vtt-cr-cr-lf-ends-block": f"WEBVTT\n\n{V1}\nA\r\r\nB\n{V2}\nC\n\n",
    "vtt-cr-then-crlf-ends-block": f"WEBVTT\rX\r\n{V1}\nA\rB\r\n{V2}\nC\n\n",
    "vtt-lone-cr-kept": f"WEBVTT\n\n{V1}\nA\rB\nC\r\n\rD\n\n",
    "vtt-cr-only-lines": f"WEBVTT\n\n{V1}\nA\n\r\r\n{V2}\nB\n\n",
    "vtt-leading-line-breaks-skipped": f"WEBVTT\n\n\r\rid\n{V1}\nA\n\n",
    "vtt-nul-ends-block": f"WEBVTT\n\n{V1}\nA\n\x00\n{V2}\nB\n\n",
    "vtt-nul-mid-line": f"WEBVTT\n\n{V1}\nA\x00B\n\n{V2}\nC\n\n",
    "vtt-nul-starts-identifier": f"WEBVTT\n\n{V1}\nA\n\x00X\n{V2}\nC\n\n",
    "vtt-nul-after-block-ends-document": f"WEBVTT\n\n{V1}\nA\n\n\x00\n\n{V2}\nB\n\n",
    "vtt-negative-duration-until-next": "WEBVTT\n\n00:05.000 --> 00:04.000\nA\n\n00:10.000 --> 00:11.000\nB\n\n",
    "vtt-long-duration": "WEBVTT\n\n00:00.000 --> 999:00:00.000\nA\n\n",
    "vtt-repeat-dropped": f"WEBVTT\n\na\n{V1} align:start\nA\n\nb\n{V2}\nB\n\nc\n{V1}\nA\n\n",
}
def capsule(codec, label, ops):
    out = bytearray(b"TRSTXT01")
    out += struct.pack(">BH", codec, len(label)) + label
    out += struct.pack(">I", len(ops))
    for op in ops:
        out += op
    return bytes(out)


def open_op(num, den, width=0, height=0, extradata=None):
    data = extradata or b""
    return b"O" + struct.pack(">IIIIBI", num, den, width, height, extradata is not None, len(data)) + data


def packet_op(pts, duration, payload):
    def known(value):
        return struct.pack(">Bq", value is not None, value if value is not None else 0)

    return b"P" + known(pts) + known(duration) + struct.pack(">I", len(payload)) + payload


def write(group, name, data):
    path = OUT / group / f"{name}.cap"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def mov_text():
    cases = json.loads((MOV_TEXT / "PROVENANCE.json").read_text())
    for name, entry in sorted(cases.items()):
        case = entry["case"]

        def local(path):
            return MOV_TEXT / path.removeprefix("cases/")

        def clock(value):
            return None if value == "unknown" else int(value)

        num, den = case["timebase"]
        config = local(case["config"]).read_bytes() if case["config"] else None
        ops = [open_op(num, den, *case["dimensions"], config)]
        for packet in case["packets"]:
            if packet == "--flush":
                ops.append(b"F")
            else:
                ops.append(packet_op(clock(case["pts"]), clock(case["duration"]), local(packet).read_bytes()))
        # Some fixture names carry the code of the run that captured them; the
        # capsules leave it out.
        write("mov-text", name.removeprefix("original177-"), capsule(1, b"", ops))
    return len(cases)


def mov_text_tags():
    num, den, pts, duration = ASS_CLOCK
    sessions = tx3g_sessions()
    for name, (description, samples) in sessions.items():
        ops = [open_op(num, den, extradata=description)]
        ops += [packet_op(pts, duration, sample) for sample in samples]
        write("mov-text-tags", name, capsule(1, b"", ops))
    return len(sessions)


def tx3g(samples):
    count = 0
    for path in sorted(Path(samples).iterdir()):
        raw, at, records = path.read_bytes(), 0, []
        while at < len(raw):
            config = None
            if raw[at]:
                (size,) = struct.unpack_from(">I", raw, at + 1)
                config = raw[at + 5 : at + 5 + size]
                at += 5 + size
            else:
                at += 1
            (size,) = struct.unpack_from(">I", raw, at)
            sample = raw[at + 4 : at + 4 + size]
            at += 4 + size
            if not records or records[-1] != (config, sample):
                records.append((config, sample))
        num, den, pts, duration = TX3G_CLOCK
        ops, current = [], object()
        for config, sample in records:
            if config != current:
                ops.append(open_op(num, den, extradata=config))
                current = config
            ops.append(packet_op(pts, duration, sample))
        write("tx3g", path.name, capsule(1, b"", ops))
        count += 1
    return count


def charset():
    text = re.sub(r"//[^\n]*", "", CHARSET.read_text())
    rows = re.findall(r'\(\s*"([^"]+)",\s*&\[([^\]]*)\],\s*&\[([^\]]*)\],', text)
    num, den, pts, duration = ASS_CLOCK
    for name, label, raw in rows:
        def literal(values):
            return bytes(int(v) for v in values.replace("\n", " ").split(",") if v.strip())

        ops = [open_op(num, den), packet_op(pts, duration, literal(raw))]
        write("charset", name, capsule(2, literal(label), ops))
    return len(rows)


def ass():
    manifest = json.loads((TEXT / "manifest.json").read_text())
    num, den, pts, duration = ASS_CLOCK
    count = 0
    for case in manifest["cases"]:
        if case["format"] != "OpaqueAss":
            continue
        private = (TEXT / case["codec_private"]).read_bytes() if case["codec_private"] else None
        ops = [open_op(num, den, extradata=private), packet_op(pts, duration, (TEXT / case["input"]).read_bytes())]
        write("ass", case["id"], capsule(2, b"", ops))
        count += 1
    return count


def authored():
    num, den, pts, duration = ASS_CLOCK
    for name, packet in ASS_PACKETS.items():
        write("ass", name, capsule(2, b"", [open_op(num, den), packet_op(pts, duration, packet)]))
    for name, (label, packet) in SINGLE_BYTE_PACKETS.items():
        write("charset", name, capsule(2, label, [open_op(num, den), packet_op(pts, duration, packet)]))
    for directory, documents in [("documents", DOCUMENTS), ("host", HOST_DOCUMENTS)]:
        for name, text in documents.items():
            path = OUT / directory / f"{name}.{name.split('-')[0]}"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(text if isinstance(text, bytes) else text.encode())
    return len(ASS_PACKETS) + len(SINGLE_BYTE_PACKETS) + len(DOCUMENTS) + len(HOST_DOCUMENTS)


def documents():
    manifest = json.loads((TEXT / "manifest.json").read_text())
    kinds = {"SubRip": "srt", "WebVtt": "vtt"}
    for document in manifest["documents"]:
        path = (TEXT / document["input"]).relative_to(ROOT)
        yield document["id"], kinds[document["format"]], str(path), [kinds[document["format"]]]
    for name in DOCUMENTS:
        kind = name.split("-")[0]
        path = (OUT / "documents" / f"{name}.{kind}").relative_to(ROOT)
        yield name, kind, str(path), [kind]


def text_inputs():
    for group in ["mov-text", "mov-text-tags", "tx3g", "charset", "ass"]:
        for path in sorted((OUT / group).glob("*.cap")):
            yield f"{group}-{path.stem}", group, str(path.relative_to(ROOT)), ["packets"]
    yield from documents()


# Documents whose time fields leave the 32-bit range, which FFmpeg reads
# differently by target: the cases of subtitle-text-host, pinned per target.
HOST_FIELDS = [
    "2147483648", "4294967295", "4294967296", "4294967297", "9223372036854775808",
    "18446744073709551615", "18446744073709551616", "99999999999999999999",
    "-2147483649", "-4294967297",
]
HOST_DOCUMENTS = {
    **{f"srt-hours-{field.replace('-', 'minus-')}": f"1\n{field}:00:01,000 --> 00:00:02,000\nA\n\n" for field in HOST_FIELDS},
    **{f"srt-seconds-{field.replace('-', 'minus-')}": f"1\n00:00:{field},000 --> 00:00:02,000\nA\n\n" for field in HOST_FIELDS},
    **{f"vtt-seconds-{field}": f"WEBVTT\n\n00:{field}.000 --> 00:02.000\nA\n\n"
       for field in ["4294967296", "4294967297", "9223372036854775808"]},
}


def host_inputs():
    for name in HOST_DOCUMENTS:
        kind = name.split("-")[0]
        path = (OUT / "host" / f"{name}.{kind}").relative_to(ROOT)
        yield name, kind, str(path), [kind]


def pgs_inputs():
    for path in sorted(PGS.glob("*/input.bin")):
        yield path.parent.name, path.parent.name.split("--")[0], str(path.relative_to(ROOT)), []


def record(feature, reference, target=None):
    inputs = {"subtitle-text": text_inputs, "subtitle-text-host": host_inputs}.get(feature, pgs_inputs)()
    prefix = "subtitle-text-" if feature.startswith("subtitle-text") else "pgs-"
    path = ROOT / "harness/features" / feature / "corpus.json"
    earlier = {}
    if target is not None and path.exists():
        earlier = {case["id"]: case["expected_by_target"] for case in json.loads(path.read_text())["cases"]}
    cases = []
    for case_id, group, file, arguments in inputs:
        result = subprocess.run(
            [reference, prefix + "fold", file, *arguments], capture_output=True, check=True
        )
        fold = json.loads(result.stdout.decode().strip().splitlines()[-1])
        if fold.get("eof") is not True:
            raise SystemExit(f"{case_id}: reference did not reach the end")
        expected = {key: fold[key] for key in ["packets", "bytes", "digest"]}
        case = {
            "id": case_id,
            "profile": case_id,
            "group": group,
            "file": file,
            "sha256": hashlib.sha256((ROOT / file).read_bytes()).hexdigest(),
            "rights": "authored-synthetic",
            "arguments": arguments,
        }
        if target is None:
            case["expected"] = expected
        else:
            pins = dict(earlier.get(case_id, {}), **{target: expected})
            case["expected_by_target"] = dict(sorted(pins.items()))
        cases.append(case)
    path.write_text(json.dumps({"schema": 1, "directory": ".", "cases": cases}, indent=2) + "\n")
    print(f"{feature}: {len(cases)} cases")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "capsules":
        counts = [mov_text(), mov_text_tags(), tx3g(sys.argv[2]), charset(), ass(), authored()]
        print("mov-text %d, mov-text-tags %d, tx3g %d, charset %d, ass %d, authored %d" % tuple(counts))
    elif len(sys.argv) == 4 and sys.argv[1] == "record" and sys.argv[2] != "subtitle-text-host":
        record(sys.argv[2], sys.argv[3])
    elif len(sys.argv) == 5 and sys.argv[1] == "record" and sys.argv[2] == "subtitle-text-host":
        record(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
