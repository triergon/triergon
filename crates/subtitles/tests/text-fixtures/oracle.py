#!/usr/bin/env python3
"""Independent source-fact oracle over the 40 authored packet inputs.

This code never imports author.py, Rust or a native media library. It checks the
declared extraction profile, not CSS/ASS rendering or a complete WebVTT UA.
"""
import argparse
import hashlib
import json
import struct
from fractions import Fraction
from pathlib import Path

ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures"


class SourceError(Exception):
    pass


def ratio(value):
    return None if value is None else [value.numerator, value.denominator]


def field(owner, start, end):
    return dict(owner=owner, range=[start, end])


def cue(raw_range, text_range, decoded, encoding, start, duration):
    end = None if start is None or duration is None else start + duration
    return dict(kind="Cue", raw=raw_range, text=text_range, decoded=decoded,
                encoding=encoding, start=ratio(start), end=ratio(end), duration=ratio(duration),
                identifier=None, settings=None, comments=None, modifiers=[])


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
