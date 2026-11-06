"""Timing boundary differential regressions, requiring built worker identities."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from .runner import ROOT, differences
from .spec_oracle import timing_trace
from tools import generate_corpus as fixture


def document(scale, duration, delay, group_ticks, frames, relative=0):
    e, u = fixture.element, fixture.uint
    header = e(0x1A45DFA3, u(0x4286, 1) + u(0x42F7, 1) + u(0x42F2, 4) + u(0x42F3, 8) + e(0x4282, b"matroska") + u(0x4287, 4) + u(0x4285, 2))
    track = u(0xD7, 1) + u(0x73C5, 1) + u(0x83, 2) + e(0x86, b"A_PCM/INT/LIT") + u(0x56AA, delay)
    if duration is not None:
        track += u(0x23E383, duration)
    track += e(0xE1, u(0x9F, 1) + u(0x6264, 16) + e(0xB5, bytes.fromhex("40bf400000000000")))
    block = b"\x81" + relative.to_bytes(2, "big", signed=True) + bytes([0x84 if frames > 1 else 0x80])
    if frames > 1:
        block += bytes([frames - 1])
    block += b"\x00\x01" * frames
    payload = e(0xA3, block) if group_ticks is None else e(0xA0, e(0xA1, block) + u(0x9B, group_ticks))
    return header + e(0x18538067, e(0x1549A966, u(0x2AD7B1, scale)) + e(0x1654AE6B, e(0xAE, track)) + e(0x1F43B675, u(0xE7, 0) + payload))


class TimingContractTests(unittest.TestCase):
    def test_exact_source_oracle_keeps_sub_tick_and_negative_values(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "source.mkv"
            path.write_bytes(document(1_000_000, 3_500_000, 2_500_000, None, 3))
            packets = timing_trace(path)["packets"]
            self.assertEqual([p["pts"]["numerator"] for p in packets], [-2_500_000, 1_000_000, 4_500_000])
            self.assertEqual(packets[0]["duration"], {"numerator": 3_500_000, "denominator": 1})

    def test_independent_source_oracle_keeps_fractional_lace_duration(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fractional.mkv"
            path.write_bytes(document(1_000_000, None, 0, 1, 3))
            packet = timing_trace(path)["packets"][2]
            self.assertEqual(packet["pts"], {"numerator": 2_000_000, "denominator": 3})
            self.assertEqual(packet["duration"], {"numerator": 1_000_000, "denominator": 3})


def differential():
    cases = [
        (1_000_000, 3_500_000, 2_500_000, None, 3, 0),
        (1_000_000, None, 0, 1, 3, 0),
        (1_000_000, None, 0, 5, 3, 0),
        (1_000_000, 2_000_000, 0, 0, 3, 0),
        (1_000_000, None, 0, None, 3, 0),
        (625_000, 1_234_567, 312_500, None, 3, 0),
        (1_000_000, 1_000_000, 0, None, 1, -4),
    ]
    from .runner import parity
    results = []
    out = ROOT / ".research/timing-boundaries"
    out.mkdir(parents=True, exist_ok=True)
    for index, values in enumerate(cases):
        path = out / f"boundary-{index}.mkv"
        path.write_bytes(document(*values))
        row = parity(ROOT / "target/release/media-worker", ROOT / "target/harness/ffmpeg-worker", path, out / f"boundary-{index}")
        results.append({"parameters": values, **row})
    (out / "report.json").write_text(json.dumps(results, indent=2) + "\n")
    print([(i, r["status"], r["differences"], r["source_timing_differences"]) for i, r in enumerate(results)])
    return 0 if all(r["status"] == "pass" for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(differential())
