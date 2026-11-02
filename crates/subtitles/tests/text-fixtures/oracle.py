#!/usr/bin/env python3
"""Independent source-fact oracle over the original 72 authored inputs.

This code never imports author.py, Rust or a native media library. It checks the
declared extraction profile, not CSS/ASS rendering or a complete WebVTT UA.
"""
import argparse
import hashlib
import json
import re
import struct
from fractions import Fraction
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures"


class SourceError(Exception):
    pass


def ratio(value):
    return None if value is None else [value.numerator, value.denominator]


def time_value(value, comma):
    separator = "," if comma else r"\."
    hours = r"([0-9]{2,}):" if comma else r"(?:([0-9]{2,}):)?"
    match = re.fullmatch(hours + r"([0-9]{2}):([0-9]{2})" + separator + r"([0-9]{3})", value)
    if not match:
        raise SourceError("Invalid")
    hour, minute, second, millis = match.groups()
    if int(minute) > 59 or int(second) > 59:
        raise SourceError("Invalid")
    return Fraction(((int(hour or "0") * 60 + int(minute)) * 60 + int(second)) * 1000 + int(millis), 1000)


def source_lines(raw, encoding):
    prefix = 0
    if encoding == "Utf8":
        codec = "utf-8"
        if raw.startswith(b"\xef\xbb\xbf"):
            prefix = 3
    elif encoding == "Utf16Bom":
        if raw.startswith(b"\xfe\xff"):
            codec = "utf-16-be"
        elif raw.startswith(b"\xff\xfe"):
            codec = "utf-16-le"
        else:
            raise SourceError("Invalid")
        prefix = 2
    elif encoding in ("Utf16Be", "Utf16Le"):
        codec = "utf-16-be" if encoding == "Utf16Be" else "utf-16-le"
    else:
        raise SourceError("Invalid")
    value = raw[prefix:].decode(codec, errors="strict")
    lines = []
    offset = prefix
    for match in re.finditer(r"[^\r\n]*(?:\r\n|\r|\n|$)", value):
        line = match.group()
        if not line:
            continue
        if line.endswith("\r\n"):
            content = line[:-2]
        elif line.endswith(("\r", "\n")):
            content = line[:-1]
        else:
            content = line
        end = offset + len(content.encode(codec))
        if end - offset > 65536:
            raise SourceError("Limit")
        after = offset + len(line.encode(codec))
        lines.append(dict(start=offset, end=end, after=after, value=content))
        offset = after
    return lines, codec


def field(owner, start, end):
    return dict(owner=owner, range=[start, end])


def cue(raw_range, text_range, decoded, encoding, start, duration):
    end = None if start is None or duration is None else start + duration
    return dict(kind="Cue", raw=raw_range, text=text_range, decoded=decoded,
                encoding=encoding, start=ratio(start), end=ratio(end), duration=ratio(duration),
                identifier=None, settings=None, comments=None, modifiers=[])


def document(raw, item, output):
    if item["format"] == "WebVtt" and item["encoding"] != "Utf8":
        raise SourceError("Invalid")
    lines, codec = source_lines(raw, item["encoding"])
    effective = dict(**{"utf-8": "Utf8", "utf-16-be": "Utf16Be", "utf-16-le": "Utf16Le"})[codec]
    at = 0
    if item["format"] == "WebVtt":
        if not lines or not re.fullmatch(r"WEBVTT(?:[ \t].*)?", lines[0]["value"]):
            raise SourceError("Invalid")
        output.append(dict(kind="Header", raw=[0, lines[0]["end"]]))
        at = 1
    while at < len(lines):
        if not lines[at]["value"]:
            at += 1
            continue
        first = lines[at]
        begin = first["start"]
        identifier = None
        if item["format"] == "WebVtt" and (
            first["value"] in ("STYLE", "REGION") or
            re.match(r"NOTE(?:[ \t]|$)", first["value"])
        ):
            kind = "Style" if first["value"] == "STYLE" else "Region" if first["value"] == "REGION" else "Note"
            while at < len(lines) and lines[at]["value"]:
                at += 1
            output.append(dict(kind=kind, raw=[begin, lines[at - 1]["end"]]))
            continue
        if item["format"] == "SubRip":
            if not re.fullmatch(r"[0-9]+", first["value"]):
                raise SourceError("Invalid")
            identifier = field("Payload", first["start"], first["end"])
            at += 1
        elif "-->" not in first["value"]:
            identifier = field("Payload", first["start"], first["end"])
            at += 1
        if at >= len(lines):
            raise SourceError("Invalid")
        timing = lines[at]
        match = re.fullmatch(r"([^ \t]+)[ \t]+-->[ \t]+([^ \t]+)(?:[ \t]+(.*))?", timing["value"])
        if not match:
            raise SourceError("Invalid")
        start = time_value(match.group(1), item["format"] == "SubRip")
        end = time_value(match.group(2), item["format"] == "SubRip")
        if end <= start:
            raise SourceError("Invalid")
        settings = None
        if match.group(3) is not None:
            if item["format"] == "SubRip":
                raise SourceError("Invalid")
            a = timing["start"] + len(timing["value"][:match.start(3)].encode(codec))
            settings = field("Payload", a, timing["end"])
        at += 1
        text_start = timing["after"]
        text_end = text_start
        while at < len(lines) and lines[at]["value"]:
            text_end = lines[at]["end"]
            at += 1
        event = cue([begin, text_end], [text_start, text_end],
                    raw[text_start:text_end].decode(codec).encode(), effective,
                    start, end - start)
        event.update(identifier=identifier, settings=settings)
        output.append(event)


def packet_timing(item):
    if item["clock"] is not None:
        clock = item["clock"]
        base = Fraction(clock["numerator"], clock["denominator"])
        start = None if clock["pts_ticks"] is None else clock["pts_ticks"] * base
        duration = None if clock["duration_ticks"] is None else clock["duration_ticks"] * base
        assert ratio(start) == item["start"]
        assert ratio(duration) == item["duration"]
        return start, duration
    return (None if item["start"] is None else Fraction(*item["start"]),
            None if item["duration"] is None else Fraction(*item["duration"]))


def operation(item):
    output = []
    raw = (FIXTURES / item["input"]).read_bytes()
    try:
        if len(raw) > (item["input_limit"] if item["input_limit"] is not None else 4 * 1024 * 1024):
            raise SourceError("Limit")
        if item["format"] in ("SubRip", "WebVtt"):
            document(raw, item, output)
            return output, None
        start, duration = packet_timing(item)
        if item["format"] == "OpaqueAss":
            output.append(cue([0, len(raw)], [0, len(raw)], None, "Opaque", start, duration))
        elif item["format"] in ("Utf8Packet", "MatroskaWebVtt"):
            decoded = raw.decode("utf-8", errors="strict").encode()
            event = cue([0, len(raw)], [0, len(raw)], decoded, "Utf8", start, duration)
            addition = item["addition"]
            if item["format"] == "MatroskaWebVtt" and addition is not None and addition["id"] == 1:
                extra = (FIXTURES / addition["path"]).read_bytes()
                extra.decode("utf-8", errors="strict")
                first = extra.find(b"\n")
                second = extra.find(b"\n", first + 1) if first >= 0 else -1
                if first < 0 or second < 0:
                    raise SourceError("Truncated")
                event["settings"] = None if first == 0 else field("Addition", 0, first)
                event["identifier"] = None if second == first + 1 else field("Addition", first + 1, second)
                event["comments"] = None if len(extra) == second + 1 else field("Addition", second + 1, len(extra))
            output.append(event)
        elif item["format"] == "Tx3g":
            if len(raw) < 2:
                raise SourceError("Truncated")
            length = struct.unpack_from(">H", raw)[0]
            end = 2 + length
            if end > len(raw):
                raise SourceError("Truncated")
            text = raw[2:end]
            if text.startswith(b"\xfe\xff"):
                decoded = text[2:].decode("utf-16-be", errors="strict").encode()
                encoding = "Utf16Be"
            else:
                decoded = text.decode("utf-8", errors="strict").encode()
                encoding = "Utf8"
            event = cue([0, len(raw)], [2, end], decoded, encoding, start, duration)
            while end < len(raw):
                if len(raw) - end < 8:
                    raise SourceError("Truncated")
                size, kind = struct.unpack_from(">I4s", raw, end)
                if size == 1:
                    raise SourceError("Unsupported:tx3g extended box size")
                if size == 0:
                    raise SourceError("Unsupported:tx3g box-to-end size")
                if size < 8:
                    raise SourceError("Invalid")
                if end + size > len(raw):
                    raise SourceError("Truncated")
                event["modifiers"].append(dict(kind=kind.hex(), range=[end, end + size]))
                end += size
            output.append(event)
        else:
            raise AssertionError("undeclared format")
        return output, None
    except UnicodeError:
        return output, "Invalid"
    except SourceError as error:
        return output, str(error)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    manifest = json.loads((FIXTURES / "manifest.json").read_text())
    findings = []
    for entry in manifest["files"]:
        data = (ROOT / entry["path"]).read_bytes()
        if len(data) != entry["bytes"] or hashlib.sha256(data).hexdigest() != entry["sha256"]:
            findings.append(dict(path=entry["path"], problem="authored file identity"))
    results = []
    for item in manifest["cases"]:
        expected = item["events"] or []
        expected = [dict(event, decoded=(FIXTURES / event["decoded"]).read_bytes())
                    if event.get("decoded") is not None else dict(event) for event in expected]
        observed, error = operation(item)
        equal = observed == expected and error == item["error"]
        results.append(dict(id=item["id"], agree=equal, expected_error=item["error"],
                            oracle_error=error, expected_events=len(expected), oracle_events=len(observed)))
        if not equal:
            def printable(value):
                if isinstance(value, bytes):
                    return dict(bytes=len(value), sha256=hashlib.sha256(value).hexdigest())
                raise TypeError(type(value).__name__)
            findings.append(dict(id=item["id"], expected=json.loads(json.dumps(expected, default=printable)),
                                 observed=json.loads(json.dumps(observed, default=printable))))
    report = dict(schema="m05-text-independent-source-oracle-check-v1",
                  cases=len(results), passed=sum(item["agree"] for item in results), findings=findings,
                  source_operation="Independent authored source-fact oracle; no Rust or native execution",
                  no_conformance_or_qualification_claim=True, results=results)
    data = json.dumps(report, indent=2) + "\n"
    if args.output is not None:
        args.output.write_text(data)
    print(json.dumps(dict(cases=report["cases"], passed=report["passed"], findings=len(findings))))
    raise SystemExit(bool(findings))


if __name__ == "__main__":
    main()
