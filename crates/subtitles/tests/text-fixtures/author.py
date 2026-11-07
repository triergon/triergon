#!/usr/bin/env python3
"""Author original text fixtures from named components, not a decoder.

Only --write creates files; otherwise this reconstructs authored components and
checks the frozen bytes. Independent parsing belongs to oracle.py, not this builder.
"""
import argparse
import hashlib
import json
import subprocess
from fractions import Fraction
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FILES = {}
CASES = []


def put(path, data):
    if isinstance(data, str):
        data = data.encode("utf-8")
    FILES[path] = data
    return path


def rational(milliseconds):
    value = Fraction(milliseconds, 1000)
    return [value.numerator, value.denominator]


def cue(raw, text, value, start=None, end=None, encoding="Utf8", **extra):
    event = dict(kind="Cue", raw=raw, text=text, decoded=value,
                 start=None if start is None else rational(start),
                 end=None if end is None else rational(end),
                 duration=None if start is None or end is None else rational(end - start),
                 encoding=encoding, identifier=None, settings=None, comments=None,
                 modifiers=[])
    event.update(extra)
    return event


def case(name, fmt, payload, events=None, error=None, encoding="Utf8", **extra):
    suffix = {"SubRip": "srt", "WebVtt": "vtt", "Tx3g": "tx3g",
              "Utf8Packet": "txt", "MatroskaWebVtt": "block",
              "OpaqueAss": "ass-packet"}[fmt]
    item = dict(id=name, format=fmt, encoding=encoding,
                input=put(f"input/{name}.{suffix}", payload), events=events,
                error=error, codec_id=None, codec_private=None, charset_label=None,
                addition=None, clock=None, start=None, duration=None,
                input_limit=None,
                classification="authored source profile; no native or renderer claim")
    item.update(extra)
    for field in ("codec_id", "codec_private", "charset_label"):
        if isinstance(item[field], bytes):
            item[field] = put(f"config/{name}.{field}", item[field])
    if item["addition"] is not None:
        identifier, data = item["addition"]
        item["addition"] = dict(id=identifier, path=put(f"addition/{name}.bin", data))
    if events is not None:
        for at, event in enumerate(events):
            value = event.get("decoded")
            if value is not None:
                event["decoded"] = put(f"expected/{name}.{at}.utf8", value)
    CASES.append(item)
    return item


class Document:
    def __init__(self, encoding="Utf8", byte_order="be", bom=False, eol="\n"):
        self.encoding = encoding
        self.codec = "utf-8" if encoding == "Utf8" else f"utf-16-{byte_order}"
        self.effective = "Utf8" if encoding == "Utf8" else "Utf16" + byte_order.title()
        self.data = bytearray()
        self.events = []
        self.eol = eol
        if bom:
            self.data.extend(b"\xef\xbb\xbf" if encoding == "Utf8" else
                             b"\xfe\xff" if byte_order == "be" else b"\xff\xfe")

    def add(self, value):
        begin = len(self.data)
        self.data.extend(value.encode(self.codec))
        return [begin, len(self.data)]

    def srt(self, number, first, last, text, lex_first=None, lex_last=None):
        begin = len(self.data)
        identifier = self.add(str(number))
        self.add(self.eol)
        self.add((lex_first or stamp(first, comma=True)) + " --> " +
                 (lex_last or stamp(last, comma=True)) + self.eol)
        span = self.add(text)
        self.events.append(cue([begin, span[1]], span, text, first, last,
                               self.effective,
                               identifier=dict(owner="Payload", range=identifier)))
        self.add(self.eol + self.eol)

    def header(self, comment=""):
        self.add("WEBVTT" + comment)
        self.events.append(dict(kind="Header", raw=[0, len(self.data)]))
        self.add(self.eol + self.eol)

    def metadata(self, kind, body):
        span = self.add(body)
        self.events.append(dict(kind=kind, raw=span))
        self.add(self.eol + self.eol)

    def vtt(self, first, last, text, identifier=None, settings=None,
            lex_first=None, lex_last=None, separator=" "):
        begin = len(self.data)
        identifier_span = None
        if identifier is not None:
            identifier_span = self.add(identifier)
            self.add(self.eol)
        self.add((lex_first or stamp(first)) + separator + "-->" + separator +
                 (lex_last or stamp(last)))
        settings_span = None
        if settings is not None:
            self.add(" ")
            settings_span = self.add(settings)
        self.add(self.eol)
        span = self.add(text)
        self.events.append(cue([begin, span[1]], span, text, first, last,
                               identifier=None if identifier_span is None else
                               dict(owner="Payload", range=identifier_span),
                               settings=None if settings_span is None else
                               dict(owner="Payload", range=settings_span)))
        self.add(self.eol + self.eol)


def stamp(ms, comma=False):
    hours, remain = divmod(ms, 3_600_000)
    minutes, remain = divmod(remain, 60_000)
    seconds, millis = divmod(remain, 1000)
    return f"{hours:02}:{minutes:02}:{seconds:02}{',' if comma else '.'}{millis:03}"


def document_case(name, fmt, document, **extra):
    return case(name, fmt, bytes(document.data), document.events,
                encoding=document.encoding, **extra)


def author_srt():
    document = Document()
    document.srt(1, 1001, 2509, "First authored cue.")
    document_case("srt-basic", "SubRip", document)
    document = Document(eol="\r\n")
    document.srt(42, 0, 3000, "First line\r\nSecond <i>line</i>")
    document_case("srt-crlf-multiline", "SubRip", document)
    document = Document()
    document.srt(7, 3, 999, "Cafe\u0301 / שָׁלוֹם / 水")
    document_case("srt-unicode-no-normalization", "SubRip", document)
    document = Document()
    document.srt(9, 5000, 9000, "later-number-first")
    document.srt(2, 5000, 6000, "same start, shorter duration")
    document_case("srt-overlap-order", "SubRip", document)
    document = Document()
    document.srt(1, 360_000_001, 360_001_002, "hundred hours")
    document_case("srt-wide-hours-profile", "SubRip", document,
                  classification="declared SubRip profile: two or more hour digits")
    for name, encoding, byte_order, bom, value in [
        ("srt-utf8-bom", "Utf8", "be", True, "BOM retained in original"),
        ("srt-utf16be-bom", "Utf16Bom", "be", True, "Å and 水"),
        ("srt-utf16le-bom", "Utf16Bom", "le", True, "surrogate pair: 😀"),
        ("srt-explicit-utf16be", "Utf16Be", "be", False, "explicit byte order"),
    ]:
        document = Document(encoding, byte_order, bom)
        document.srt(1, 12, 1012, value)
        document_case(name, "SubRip", document)
    for name, payload, error, encoding in [
        ("srt-end-before-start", b"1\n00:00:02,000 --> 00:00:01,000\nx\n\n", "Invalid", "Utf8"),
        ("srt-sequence-missing", b"00:00:01,000 --> 00:00:02,000\nx\n\n", "Invalid", "Utf8"),
        ("srt-fraction-width", b"1\n00:00:01,00 --> 00:00:02,000\nx\n\n", "Invalid", "Utf8"),
        ("srt-overlong-utf8", b"1\n00:00:01,000 --> 00:00:02,000\n\xc0\xaf\n\n", "Invalid", "Utf8"),
        ("srt-lone-surrogate", b"\xfe\xff" + "1\n00:00:01,000 --> 00:00:02,000\n".encode("utf-16-be") + b"\xd8\x00\x00\n\x00\n", "Invalid", "Utf16Bom"),
    ]:
        case(name, "SubRip", payload, error=error, encoding=encoding,
             classification="malformed within declared strict SubRip ingestion profile")


def author_vtt():
    document = Document()
    document.header()
    document.vtt(1250, 2750, "short timestamp", lex_first="00:01.250", lex_last="00:02.750")
    document_case("vtt-short-timestamps", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(0, 1000, "authored positioning", identifier="cue:7", settings="line:25% position:10%,line-left size:80% align:start")
    document_case("vtt-id-settings", "WebVtt", document)
    for name, eol, text in [
        ("vtt-crlf-multiline", "\r\n", "a\r\nb"),
        ("vtt-cr-lines", "\r", "left\rright"),
    ]:
        document = Document(eol=eol)
        document.header()
        document.vtt(0, 1000, text)
        document_case(name, "WebVtt", document)
    document = Document(bom=True)
    document.header()
    document.vtt(0, 1000, "optional file BOM")
    document_case("vtt-utf8-bom", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(0, 1000, "<v Ana><c.lime>Cafe\u0301 &amp; 水</c></v><00:00.500>end")
    document_case("vtt-payload-markup-exact", "WebVtt", document)
    document = Document()
    document.header()
    document.metadata("Note", "NOTE original comment\nretained verbatim")
    document.metadata("Style", "STYLE\n::cue { color: lime; }")
    document.metadata("Region", "REGION\nid:r1\nwidth:40%\nlines:2")
    document.vtt(0, 1000, "region cue", settings="region:r1")
    document_case("vtt-metadata-order", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(0, 1000, "before")
    document.metadata("Note", "NOTE between cues")
    document.vtt(1000, 2000, "after")
    document_case("vtt-note-between-cues", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(1, 999, "tabs are timing separators", separator="\t")
    document_case("vtt-tab-separators", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(360_000_001, 360_001_002, "hundred hours")
    document_case("vtt-wide-hours", "WebVtt", document)
    document = Document()
    document.header()
    document.vtt(0, 3000, "overlap first")
    document.vtt(0, 2000, "overlap second")
    document_case("vtt-overlap-order", "WebVtt", document)
    document = Document()
    document.header(" authored language facts")
    document.vtt(0, 1000, "header annotation")
    document_case("vtt-header-annotation", "WebVtt", document)
    document = Document()
    document.header()
    document_case("vtt-empty-document", "WebVtt", document)
    for name, payload, error, encoding in [
        ("vtt-bad-signature", b"WEBVTTX\n\n", "Invalid", "Utf8"),
        ("vtt-utf16-refused", "WEBVTT\n\n".encode("utf-16-be"), "Invalid", "Utf16Be"),
        ("vtt-zero-interval-strict", b"WEBVTT\n\n00:01.000 --> 00:01.000\nx\n\n", "Invalid", "Utf8"),
        ("vtt-surrogate-utf8", b"WEBVTT\n\n00:01.000 --> 00:02.000\n\xed\xa0\x80\n\n", "Invalid", "Utf8"),
        ("vtt-missing-fraction", b"WEBVTT\n\n00:01 --> 00:02\nx\n\n", "Invalid", "Utf8"),
    ]:
        prefix = [dict(kind="Header", raw=[0, 6])] if name in (
            "vtt-zero-interval-strict", "vtt-missing-fraction") else []
        case(name, "WebVtt", payload, events=prefix, error=error, encoding=encoding,
             classification="strict ingestion error; not a claim about user-agent recovery")


def box(kind, body):
    return (8 + len(body)).to_bytes(4, "big") + kind + body


def txcase(name, data, decoded, modifiers=(), **extra):
    payload = bytearray(len(data).to_bytes(2, "big") + data)
    records = []
    for kind, body in modifiers:
        begin = len(payload)
        payload.extend(box(kind, body))
        records.append(dict(kind=kind.hex(), range=[begin, len(payload)]))
    effective = "Utf16Be" if data.startswith(b"\xfe\xff") else "Utf8"
    event = cue([0, len(payload)], [2, 2 + len(data)], decoded, 1, 1002,
                effective, modifiers=records)
    return case(name, "Tx3g", bytes(payload), [event], encoding="Tx3gInBand",
                start=rational(1), duration=rational(1001), codec_id=b"tx3g", **extra)


def author_tx3g():
    txcase("tx3g-basic", b"authored", "authored")
    txcase("tx3g-byte-not-character-count", "Å水é".encode(), "Å水é")
    txcase("tx3g-utf16be-bom", b"\xfe\xff" + "Å水\né".encode("utf-16-be"), "Å水\né")
    txcase("tx3g-empty-sample", b"", "", classification="zero text bytes, no invented clear/EOF event")
    txcase("tx3g-embedded-nul", b"a\x00b", "a\x00b")
    txcase("tx3g-line-codepoints-exact", b"a\r\nb\rc\nd", "a\r\nb\rc\nd")
    style = b"\x00\x01" + b"\x00\x00\x00\x03\x00\x01\x01\x14\x12\x34\x56\x78"
    txcase("tx3g-style-highlight-raw", b"abc", "abc",
           [(b"styl", style), (b"hlit", b"\x00\x01\x00\x03"),
            (b"hclr", b"\x11\x22\x33\x44")])
    txcase("tx3g-unknown-modifier", b"x", "x", [(b"zZ09", b"\xff\x00opaque")])
    txcase("tx3g-modifier-order", b"abc", "abc",
           [(b"twrp", b"\x00"), (b"twrp", b"\x01")],
           classification="source modifier order retained; semantic precedence not invented")
    txcase("tx3g-empty-with-modifier", b"", "", [(b"tbox", b"\x00" * 8)])
    txcase("tx3g-u16-byte-count-boundary", b"a" * 65535, "a" * 65535,
           classification="u16 syntax limit; exceeds 2048-byte author SHOULD interoperability guidance")
    for name, payload, error in [
        ("tx3g-missing-length", b"\x00", "Truncated"),
        ("tx3g-text-overrun", b"\x00\x05xy", "Truncated"),
        ("tx3g-utf16-odd", b"\x00\x03\xfe\xff\x00", "Invalid"),
        ("tx3g-lone-surrogate", b"\x00\x04\xfe\xff\xd8\x00", "Invalid"),
        ("tx3g-short-box", b"\x00\x01x\x00\x00\x00\x07zZ09", "Invalid"),
        ("tx3g-box-overrun", b"\x00\x01x\x00\x00\x00\x10zZ09\x00", "Truncated"),
        ("tx3g-extended-box-gap", b"\x00\x01x\x00\x00\x00\x01zZ09\x00\x00\x00\x00\x00\x00\x00\x10", "Unsupported:tx3g extended box size"),
    ]:
        case(name, "Tx3g", payload, error=error, encoding="Tx3gInBand",
             classification="malformed sample or explicitly unsupported inherited box form")


def packet(name, fmt, payload, text=None, start=None, duration=None, **extra):
    if isinstance(payload, str):
        payload = payload.encode()
    end = None if start is None or duration is None else [
        (Fraction(*start) + Fraction(*duration)).numerator,
        (Fraction(*start) + Fraction(*duration)).denominator]
    event = cue([0, len(payload)], [0, len(payload)], text,
                encoding="Opaque" if fmt == "OpaqueAss" else "Utf8")
    event.update(start=start, end=end, duration=duration)
    return case(name, fmt, payload, [event], start=start, duration=duration, **extra)


def author_packets():
    packet("utf8-packet-markup", "Utf8Packet", "<i>literal markup</i>", "<i>literal markup</i>", [1, 3], [2, 3], codec_id=b"S_TEXT/UTF8")
    packet("utf8-packet-unknown-time", "Utf8Packet", "unknown", "unknown")
    packet("utf8-packet-known-zero-duration", "Utf8Packet", "zero", "zero", [0, 1], [0, 1])
    packet("utf8-packet-negative-start", "Utf8Packet", "edit", "edit", [-5, 7], [1, 3])
    packet("utf8-packet-raw-clock", "Utf8Packet", "clock", "clock", [1, 30000], [1001, 90000],
           clock=dict(numerator=2, denominator=180000, pts_ticks=3, dts_ticks=-6, duration_ticks=1001))
    packet("utf8-packet-bom-nul-content", "Utf8Packet", b"\xef\xbb\xbfa\x00b", "\ufeffa\x00b")
    case("utf8-packet-too-large-scalar", "Utf8Packet", b"\xf4\x90\x80\x80", error="Invalid", classification="RFC3629 scalar beyond U+10FFFF")
    case("utf8-packet-input-limit", "Utf8Packet", b"abcdef", error="Limit", input_limit=3,
         classification="local resource policy, not format invalidity")

    private = b"WEBVTT\n\nSTYLE\n::cue { color: lime; }\n\n"
    packet("mkv-vtt-no-addition", "MatroskaWebVtt", "stored text", "stored text", [5, 2], [3, 4], codec_private=private, codec_id=b"S_TEXT/WEBVTT")
    item = packet("mkv-vtt-settings-id-comments", "MatroskaWebVtt", "body", "body", [1, 1], [1, 2],
                  codec_private=private, addition=(1, b"align:start\nidentifier-4\nNOTE authored before cue\n\n"))
    item["events"][0].update(settings=dict(owner="Addition", range=[0, 11]),
                              identifier=dict(owner="Addition", range=[12, 24]),
                              comments=dict(owner="Addition", range=[25, 51]))
    # End offsets above are checked independently against actual source slices.
    packet("mkv-vtt-present-empty-fields", "MatroskaWebVtt", "empty fields", "empty fields", addition=(1, b"\n\n"), codec_private=b"")
    packet("mkv-vtt-relative-inline-time", "MatroskaWebVtt", "<00:00.250>relative", "<00:00.250>relative", [10, 1], [1, 1])
    packet("mkv-vtt-unknown-addition-opaque", "MatroskaWebVtt", "source", "source", addition=(19, b"unknown mapping\x00\xff"),
           classification="opaque non-codec BlockAddition retained without interpreting its unknown mapping")
    case("mkv-vtt-truncated-addition", "MatroskaWebVtt", b"body", error="Truncated", addition=(1, b"align:start\nmissing identifier terminator"))

    packet("ass-raw-order-overrides", "OpaqueAss", b"9,2,Default,Ana,0000,0000,0000,,{\\an7}literal,comma\\Nsecond", start=[0, 1], duration=[1, 1], codec_id=b"S_TEXT/ASS", charset_label=b"UTF-8")
    packet("ass-ssa-empty-layer", "OpaqueAss", b"3,,Default,,0000,0000,0000,,SSA packet", codec_id=b"S_TEXT/SSA")
    packet("ass-private-verbatim", "OpaqueAss", "1,0,Default,,0,0,0,,Cafe\u0301", codec_private=b"\xef\xbb\xbf[Script Info]\r\nScriptType: v4.00+\r\n[V4+ Styles]\r\n", charset_label=b"uTf-8")
    packet("ass-unknown-charset-opaque", "OpaqueAss", b"2,0,Default,,0,0,0,,\xcf\xf0\xe8\xe2\xe5\xf2", charset_label=b"windows-1251", encoding="Opaque", classification="unvalidated opaque source; no Matroska UTF-8 or charset-decoding conformance claim")
    packet("ass-empty-not-clear", "OpaqueAss", b"", start=[0, 1], duration=[0, 1], classification="empty opaque event, not invented renderer clear or EOF")
    packet("ass-known-duration-unknown-start", "OpaqueAss", b"4,0,Default,,0,0,0,,unknown PTS", duration=[1, 3])
    packet("ass-private-present-empty", "OpaqueAss", b"5,0,Default,,0,0,0,,empty config", codec_private=b"")
    packet("ass-invalid-utf8-unvalidated", "OpaqueAss", b"6,0,Default,,0,0,0,,\xff", charset_label=b"UTF-8", classification="opaque carriage does not assert declared encoding is truthful or source conforming")


def rs_string(value):
    # JSON escapes are a Rust subset for these ASCII labels/paths.
    assert value.isascii()
    return json.dumps(value)


def rs_file(path):
    return f"Some(include_bytes!({rs_string(path)}))"


def rs_time(value):
    return f"Some(({value[0]}_i128, {value[1]}_u128))"


def rs_field(field):
    return ("Some(ExpectedField { owner: FieldOwner::" + field["owner"] +
            f", range: ({field['range'][0]}, {field['range'][1]}) }})")


def rs_clock(clock):
    return "Some(RawClock { " + ", ".join(
        f"{key}: " + ("None" if value is None else f"Some({value})")
        if key.endswith("ticks") else f"{key}: {value}"
        for key, value in clock.items()) + " })"


def rs_some(lines, indent, field, value, render):
    # Absent values come from EMPTY and EMPTY_EVENT in tests/text.rs.
    if value is not None:
        lines.append(f"{indent}{field}: {render(value)},")


def rust_cases():
    lines = ["// Generated only from original authored components by author.py.",
             "use super::*;", "pub(super) static ALL: &[Case] = &["]
    for item in CASES:
        lines.append("    Case {")
        for field in ("id", "format", "encoding"):
            value = rs_string(item[field]) if field == "id" else field.title() + "::" + item[field]
            lines.append(f"        {field}: {value},")
        lines.append(f"        payload: include_bytes!({rs_string(item['input'])}),")
        for field in ("codec_id", "codec_private", "charset_label"):
            rs_some(lines, " " * 8, field, item[field], rs_file)
        rs_some(lines, " " * 8, "addition", item["addition"], lambda addition:
                f"Some(({addition['id']}, include_bytes!({rs_string(addition['path'])})))")
        for field in ("start", "duration"):
            rs_some(lines, " " * 8, field, item[field], rs_time)
        rs_some(lines, " " * 8, "clock", item["clock"], rs_clock)
        rs_some(lines, " " * 8, "input_limit", item["input_limit"], lambda limit: f"Some({limit})")
        rs_some(lines, " " * 8, "error", item["error"], lambda error: f"Some({rs_string(error)})")
        if item["events"]:
            lines.append("        events: &[")
        for event in item["events"] or []:
            lines.append("            ExpectedEvent { kind: ExpectedKind::" + event["kind"] + ",")
            for field in ("raw", "text"):
                pair = event.get(field, [0, 0])
                lines.append(f"                {field}: ({pair[0]}, {pair[1]}),")
            rs_some(lines, " " * 16, "decoded", event.get("decoded"), rs_file)
            lines.append(f"                encoding: Encoding::{event.get('encoding', 'Utf8')},")
            for field in ("identifier", "settings", "comments"):
                rs_some(lines, " " * 16, field, event.get(field), rs_field)
            for field in ("start", "end", "duration"):
                rs_some(lines, " " * 16, field, event.get(field), rs_time)
            mods = []
            for modifier in event.get("modifiers", []):
                kind = list(bytes.fromhex(modifier["kind"]))
                a, b = modifier["range"]
                mods.append(f"([{', '.join(map(str, kind))}], ({a}, {b}))")
            if mods:
                lines.append("                modifiers: &[" + ", ".join(mods) + "],")
            lines.append("                ..EMPTY_EVENT")
            lines.append("            },")
        if item["events"]:
            lines.append("        ],")
        lines.append("        ..EMPTY\n    },")
    lines.append("];")
    formatted = subprocess.run(["rustfmt", "--edition", "2024"], check=True, text=True,
                               input="\n".join(lines) + "\n", stdout=subprocess.PIPE)
    put("fixtures/cases.rs", formatted.stdout)
    put("fixtures/test_names.rs", "// Each authored case is an individually reported Rust test.\n" +
        "\n".join(f"fixture_test!({item['id'].replace('-', '_')}, {index});"
                  for index, item in enumerate(CASES)) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    author_srt()
    author_vtt()
    author_tx3g()
    author_packets()
    assert len(CASES) == 72, len(CASES)
    # Rust include paths in fixtures/cases.rs are relative to that file.
    rust_cases()
    for path in list(FILES):
        if not path.startswith("fixtures/"):
            FILES["fixtures/" + path] = FILES.pop(path)
    manifest = dict(schema="m05-original-text-source-fixtures-v1",
                    provenance="Original lane-authored components and expected source facts; no decoder output used",
                    cases=CASES,
                    files=[dict(path=path, bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
                           for path, data in sorted(FILES.items())])
    FILES["fixtures/manifest.json"] = (json.dumps(manifest, ensure_ascii=True, indent=2) + "\n").encode()
    for path, data in FILES.items():
        target = ROOT / path
        if args.write:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        elif not target.exists() or target.read_bytes() != data:
            raise SystemExit(f"authored bytes differ: {path}")
    print(json.dumps(dict(cases=len(CASES), files=len(FILES),
                          operation="write" if args.write else "reconstruct-check",
                          native_or_rust_executed=False)))


if __name__ == "__main__":
    main()
