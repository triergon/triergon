#!/usr/bin/env python3
"""Author the subtitle runners' inputs and record their expected folds.

    subtitle_corpus.py capsules TX3G_SAMPLES
        Write the subtitle-text session capsules under
        harness/fixtures/subtitle-text/ from repository fixtures: the captured
        mov_text cases, the samples the tx3g semantic tests build (recorded with
        TRIERGON_TX3G_SAMPLES=TX3G_SAMPLES, see
        crates/subtitles/tests/text_tx3g_semantics.rs), the recorded charset
        receiver cases and the authored ASS packets.

    subtitle_corpus.py record FEATURE REFERENCE
        Write harness/features/FEATURE/corpus.json (subtitle-text or
        subtitle-pgs-capsules), each case's expected fold recorded by running the
        reference worker REFERENCE on it.

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


EMPTY = (
    "an empty packet is an event in Triergon (text README, Behaviour and limits); "
    "avcodec_decode_subtitle2 does not decode a zero-size packet, so the reference publishes nothing"
)
OPAQUE = (
    "the opaque ASS profile (OpaqueAss1) carries the packet bytes unvalidated and unconverted; "
    "the reference refuses decoded text that is not UTF-8"
)
STRICT_SRT = "the strict SubRip profile (SubRipStrict1) refuses {}; the srt demuxer accepts it"
REFUSED_VTT = "the strict WebVTT profile (WebVttSourceStrict1) refuses {}; the webvtt demuxer reports an empty document"
# Cases where Triergon and the reference differ, with the cause. They stay in the
# corpus as failures.
DIFFERENCES = {
    **{f"charset-{label}-empty": EMPTY for label in ["big5", "cp932", "cp949", "cp950", "euc-kr", "gbk", "shift_jis"]},
    "ass-ass-empty-not-clear": EMPTY,
    "ass-ass-invalid-utf8-unvalidated": OPAQUE,
    "ass-ass-unknown-charset-opaque": OPAQUE,
    "srt-crlf-multiline": "Triergon keeps CRLF line ends in cue text (no line-ending rewriting); the srt demuxer rewrites them to LF",
    "srt-utf16be-bom": (
        "the reference's UTF-16 SubRip reader keeps one byte of the first character of the cue text "
        "(U+00C5 becomes 0xC3); Triergon decodes the character. Only the comparison-only "
        "reference_trace::carriage view reproduces the reference"
    ),
    "srt-explicit-utf16be": "the caller states UTF-16BE without a byte-order mark; the srt demuxer has no stated encoding and reads no cue",
    "srt-end-before-start": STRICT_SRT.format("an end time before the start time"),
    "srt-sequence-missing": STRICT_SRT.format("a cue without its number"),
    "srt-fraction-width": STRICT_SRT.format("a two-digit millisecond field"),
    "srt-overlong-utf8": STRICT_SRT.format("an overlong UTF-8 sequence"),
    "srt-lone-surrogate": "Triergon refuses a lone UTF-16 surrogate; the srt demuxer reads no cue and reports success",
    "vtt-cr-lines": "WebVTT allows CR line ends and Triergon reads the cue; the webvtt demuxer finds no cue",
    "vtt-bad-signature": REFUSED_VTT.format("a signature line other than WEBVTT"),
    "vtt-utf16-refused": REFUSED_VTT.format("a UTF-16 document"),
    "vtt-missing-fraction": REFUSED_VTT.format("a timestamp without milliseconds"),
    "vtt-zero-interval-strict": "the strict WebVTT profile refuses a zero-length cue; the webvtt demuxer keeps it",
    "vtt-surrogate-utf8": "Triergon refuses an encoded surrogate as invalid UTF-8; the webvtt demuxer passes the bytes through",
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


def documents():
    manifest = json.loads((TEXT / "manifest.json").read_text())
    names = {"Utf8": "utf8", "Utf16Bom": "utf16-bom", "Utf16Be": "utf16be", "Utf16Le": "utf16le"}
    kinds = {"SubRip": "srt", "WebVtt": "vtt"}
    for case in manifest["cases"]:
        if case["format"] in kinds:
            path = (TEXT / case["input"]).relative_to(ROOT)
            yield case["id"], kinds[case["format"]], str(path), [kinds[case["format"]], names[case["encoding"]]]


def text_inputs():
    for group in ["mov-text", "tx3g", "charset", "ass"]:
        for path in sorted((OUT / group).glob("*.cap")):
            yield f"{group}-{path.stem}", group, str(path.relative_to(ROOT)), ["packets"]
    yield from documents()


def pgs_inputs():
    for path in sorted(PGS.glob("*/input.bin")):
        yield path.parent.name, path.parent.name.split("--")[0], str(path.relative_to(ROOT)), []


def record(feature, reference):
    inputs = text_inputs() if feature == "subtitle-text" else pgs_inputs()
    prefix = "subtitle-text-" if feature == "subtitle-text" else "pgs-"
    cases = []
    for case_id, group, file, arguments in inputs:
        result = subprocess.run(
            [reference, prefix + "fold", file, *arguments], capture_output=True, check=True
        )
        fold = json.loads(result.stdout.decode().strip().splitlines()[-1])
        if fold.get("eof") is not True:
            raise SystemExit(f"{case_id}: reference did not reach the end")
        cases.append(
            {
                "id": case_id,
                "profile": case_id,
                "group": group,
                "file": file,
                "sha256": hashlib.sha256((ROOT / file).read_bytes()).hexdigest(),
                "rights": "authored-synthetic",
                "arguments": arguments,
                "expected": {key: fold[key] for key in ["packets", "bytes", "digest"]},
            }
        )
        if case_id in DIFFERENCES:
            cases[-1]["difference"] = DIFFERENCES[case_id]
    path = ROOT / "harness/features" / feature / "corpus.json"
    path.write_text(json.dumps({"schema": 1, "directory": ".", "cases": cases}, indent=2) + "\n")
    print(f"{feature}: {len(cases)} cases")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "capsules":
        counts = [mov_text(), tx3g(sys.argv[2]), charset(), ass()]
        print("mov-text %d, tx3g %d, charset %d, ass %d" % tuple(counts))
    elif len(sys.argv) == 4 and sys.argv[1] == "record":
        record(sys.argv[2], sys.argv[3])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
