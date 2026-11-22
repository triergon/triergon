"""Command-line entry to the independent Python oracles.

    python3 -m harness.oracles timing|content|chain|colour|catalogue FILE
    python3 -m harness.oracles KIND FILE [ARG...]
    python3 -m harness.oracles inflate

Prints {"data": ...} or, for the malformed-input errors an oracle raises on
purpose, {"error": "..."}. Any other failure exits non-zero.
"""
import json
from importlib import import_module
from pathlib import Path
import sys
import zlib

from .catalogue_oracle import source_catalogue
from .colour_oracle import source_colour
from .content_oracle import content_trace, encoding_chain
from .spec_oracle import timing_trace
from .zlib_oracle import inflate_cases

EXPECTED = (ValueError, IndexError, KeyError, StopIteration)
ORACLES = {
    "timing": (timing_trace, EXPECTED),
    "content": (content_trace, EXPECTED + (zlib.error,)),
    "chain": (encoding_chain, EXPECTED + (zlib.error,)),
    "colour": (source_colour, EXPECTED),
    "catalogue": (source_catalogue, EXPECTED),
}
# Trace oracles return canonical text rows without line endings. Load each on demand.
TRACE_ORACLES = {
    "flac-integer": ("harness.flac_integer_oracle", "trace"),
    "elementary-metadata": ("harness.elementary_oracle", "trace"),
    "hvcc-prefix": ("harness.codec_config_oracle", "hvcc_prefix"),
    "vpcc": ("harness.codec_config_oracle", "vpcc"),
    "eac3-bsi": ("harness.codec_config_oracle", "eac3_bsi"),
    "av1-obu": ("harness.codec_stream_oracle", "av1_obu"),
    "av1-low-overhead": ("harness.av1_bridge_oracle", "trace"),
    "adts": ("harness.codec_stream_oracle", "adts"),
    "pgs-segments": ("harness.pgs_segment_oracle", "trace"),
    "text-extraction": ("harness.text_extraction_oracle", "trace"),
    "tx3g-config": ("harness.tx3g_oracle", "trace"),
    "tx3g-semantics": ("harness.tx3g_semantics_oracle", "trace"),
    "opus-final-range": ("harness.opus_range_oracle", "ranges"),
    "core-budgets": ("harness.core_budget_oracle", "budget_trace"),
    "core-reader-protocol": ("harness.core_reader_oracle", "trace"),
    "source-signatures": ("harness.signature_oracle", "trace"),
    "facade-source-control": ("harness.oracles", "facade_source_control"),
    "facade-mpegts-controls": ("harness.ts_facade_oracle", "trace"),
    "mpeg1-encoder-budget": ("harness.oracles", "mpeg1_encoder_budget"),
    "playback-probe-lifecycle": ("harness.oracles", "playback_probe_lifecycle"),
    "bridge-text-control": ("harness.oracles", "bridge_text_control"),
    "ac3-features": ("harness.ac3_audblk_oracle", "trace"),
    "divide": ("harness.codec_dsp_oracle", "divide"),
    "windows": ("harness.codec_dsp_oracle", "windows"),
    "rbsp": ("harness.bitstream_oracle", "rbsp"),
    "bit-readers": ("harness.bitstream_oracle", "bit_readers"),
    "cabac-engine": ("harness.cabac_oracle", "trace"),
    "ycbcr-palette": ("harness.palette_oracle", "trace"),
    "qt-sound-v0": ("harness.isobmff_entry_oracle", "qt_sound"),
    "ddts": ("harness.isobmff_entry_oracle", "ddts"),
    "isobmff-index": ("harness.isobmff_index_oracle", "index_trace"),
    "mp4-source-clock": ("harness.mp4_source_clock_oracle", "trace"),
    "dts-header": ("harness.dts_oracle", "header"),
    "dts-unpack": ("harness.dts_oracle", "unpack"),
    "store": ("harness.store_oracle", "store_trace"),
    "mkv-metadata": ("harness.matroska_metadata_oracle", "metadata"),
    "mkv-track-extensions": ("harness.matroska_metadata_oracle", "extensions"),
    "mkv-codec-state": ("harness.matroska_metadata_oracle", "codec_state"),
    "ts-video-descriptors": ("harness.ts_descriptor_oracle", "trace"),
    "ts-packets": ("harness.ts_packet_oracle", "trace"),
    "ts-time": ("harness.ts_time_oracle", "trace"),
    "views-smoke": ("harness.oracles", "views_smoke"),
}


def views_smoke(path):
    data = path.read_bytes()
    return [f"3 {len(data)} {zlib.adler32(data)}"]


def facade_source_control(path, container):
    """The source capabilities and failure responses promised by ContainerReader."""
    size = path.stat().st_size
    rows = []
    for length in (0, size):
        if container == "mp4":
            rows.append(f"3 {length} 0")
        else:
            rows.extend((f"1 {length} 0", f"2 {length} 0"))
        rows.extend((f"1 {length} 1", f"2 {length} 1"))
    return rows


def playback_probe_lifecycle(path):
    """The documented sequential handoff and owned source callbacks."""
    from hashlib import md5

    source = path.read_bytes()
    return [
        "1 abort close",
        "2 invalidate invalidate abort close",
        f"5 {len(source)} {md5(source).hexdigest()}",
        "3 abort close",
        "4 abort close",
    ]


def mpeg1_encoder_budget(path):
    width, height = (int(field) for field in path.read_text().split())
    return ["1", "2", "3 0 1", "3 1 0", "3 2 1",
            f"4 {width} {height} {(320001 + 399) // 400} {(32001 + 16383) // 16384}"]


def bridge_text_control(_path, kind, container, codec, ordinal, program):
    """Container packet ownership and restarts from the TextBridge contract."""
    if (kind, ordinal, program) != ("text", "0", "0"):
        raise ValueError("expected the first text track")
    source = {("mov", "tx3g"): 1, ("mkv", "utf8"): 2}[container, codec]
    return [f"1 {source} 1", "2 1", "3 1", "4 22"]


# Oracles that read no input file.
GENERATORS = {
    "inflate": inflate_cases,
}


def main():
    if len(sys.argv) >= 3 and sys.argv[1] in TRACE_ORACLES:
        module, function = TRACE_ORACLES[sys.argv[1]]
        oracle = getattr(import_module(module), function)
        result = {"data": oracle(Path(sys.argv[2]), *sys.argv[3:])}
    elif len(sys.argv) == 3 and sys.argv[1] in ORACLES:
        oracle, expected = ORACLES[sys.argv[1]]
        try:
            result = {"data": oracle(Path(sys.argv[2]))}
        except expected as error:
            result = {"error": str(error)}
    elif len(sys.argv) == 2 and sys.argv[1] in GENERATORS:
        result = {"data": GENERATORS[sys.argv[1]]()}
    else:
        raise SystemExit(__doc__)
    json.dump(result, sys.stdout)


if __name__ == "__main__":
    main()
