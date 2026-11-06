"""Exercise both real collector entry points without starting measurement workers."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools import test_catalogue_vector, test_media
from .common import digest
from .comparison import reference_record, reference_summary


class CollectorReuseTests(unittest.TestCase):
    def collect(self, external, timing, memory, *, pairs=30, iterations=50,
                previous_instrumentation=None, overwrite_previous=False):
        name = "test_catalogue_vector.py" if external else "test_media.py"
        collector = test_catalogue_vector if external else test_media
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def write(path, data):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps(data))

            candidate, reference = root / "target/release/media-worker", root / "target/harness/ffmpeg-worker"
            for path in (candidate, reference):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(path.name.encode())
            identity = {"binaries": {"candidate": digest(candidate), "reference": digest(reference)},
                        "instrumentation": {"probe_sha256": "probe", "observer_sha256": "observer"}}
            write(root / "target/harness/identity.json", identity)
            fixture = root / (".research/fate-samples" if external else "fixtures") / "fixture.mkv"
            fixture.parent.mkdir(parents=True)
            fixture.write_bytes(b"authored collector fixture; measurement workers are mocked")
            feature = root / ("harness/catalogue-slice.json" if external else "feature.json")
            contract = {"id": "K5-segment-metadata-v1" if external else "K0-container-demux-v1",
                        "minimum_pairs": 30, "required_additional_vectors": [
                            {"file": "fixture.mkv", "sha256": digest(fixture), "rights": "unreviewed"}]}
            write(feature, contract)
            manifest = root / "manifest.json"
            write(manifest, {"cases": [{"id": "fixture.mkv", "profile": "fixture.mkv",
                                        "file": "fixture.mkv", "sha256": digest(fixture),
                                        "rights": "authored-synthetic"}]})
            for path in (root / "research/fate-index.json", root / "harness/fate-map.json"):
                write(path, {})
            operation = {"name": "container-demux-v1", "seek_target_ns": None,
                         "view": "catalogue-source-and-native-v1" if external else "source-frames-ffmpeg9-clock-v1"}
            row = {"id": "fixture.mkv", "profile": "fixture.mkv", "input_sha256": digest(fixture),
                   "operation": operation, "reference": reference_summary(reference_record(identity)),
                   "timing": timing, "memory": memory}
            previous = root / "previous/report.json"
            old_identity = {**identity, "instrumentation": previous_instrumentation or identity["instrumentation"]}
            write(previous, {"identity": old_identity, "contract_sha256": digest(feature),
                             **({"case": row} if external else {"cases": [row]})})
            previous_bytes = previous.read_bytes()
            output = previous.parent if overwrite_previous else root / "next"
            argv = [name, "--output", str(output), "--reuse-report", str(previous),
                    "--benchmark", "--memory", "--pairs", str(pairs), "--iterations", str(iterations)]
            if not external:
                argv += ["--corpus", str(fixture.parent), "--manifest", str(manifest), "--contract", str(feature)]
            verdict = {"accepted": False, "slice_accepted": False,
                       "reasons": ["test has no numerical evidence"], "slice_reasons": ["test has no numerical evidence"]}
            with contextlib.ExitStack() as stack:
                stack.enter_context(patch.object(collector, "ROOT", root))
                stack.enter_context(patch.object(collector, "source_identity_error", return_value=None))
                stack.enter_context(patch.object(collector, "acceptance", return_value=verdict))
                for function in (["catalogue_parity"] if external else ["parity", "fate_coverage"]):
                    stack.enter_context(patch.object(collector, function, return_value={"status": "pass"}))
                timed = stack.enter_context(patch.object(collector, "benchmark", return_value={"status": "new-timing"}))
                measured = stack.enter_context(patch.object(collector, "memory", return_value={"status": "new-memory"}))
                stack.enter_context(patch("sys.argv", argv))
                stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
                stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
                try:
                    result = collector.main()
                except SystemExit as error:
                    result = error.code
            self.assertEqual(previous.read_bytes(), previous_bytes)
            report = json.loads((output / "report.json").read_text()) if (output / "report.json").exists() else {}
            return result, report.get("case", next(iter(report.get("cases", [])), {})), timed.call_count, measured.call_count

    def test_terminal_timing_findings_are_not_resampled_on_identical_workers(self):
        for external in (False, True):
            for status in ("unequal-work", "worker-error", "timeout", "input-changed"):
                with self.subTest(external=external, status=status):
                    old = {"status": status, "raw": "first failed observation"}
                    _, row, timing_calls, _ = self.collect(external, old, {})
                    self.assertEqual(timing_calls, 0)
                    self.assertEqual(row["timing"], old)

    def test_terminal_memory_findings_are_not_resampled_on_identical_instrumentation(self):
        for external in (False, True):
            for status in ("rss-attribution-unresolved", "unequal-memory-work", "observer-unqualified", "measurement-failed"):
                with self.subTest(external=external, status=status):
                    old = {"status": status, "samples": [{"raw": "retained"}]}
                    _, row, _, memory_calls = self.collect(external, {}, old)
                    self.assertEqual(memory_calls, 0)
                    self.assertEqual(row["memory"], old)

    def test_contamination_is_retryable_in_both_collectors(self):
        for external in (False, True):
            with self.subTest(external=external):
                old = {"status": "contaminated", "samples": [{"invalid_window": True}]}
                _, row, timing_calls, memory_calls = self.collect(external, old, old)
                self.assertEqual((timing_calls, memory_calls), (1, 1))
                self.assertEqual(row["timing"]["status"], "new-timing")
                self.assertEqual(row["memory"]["status"], "new-memory")

    def test_complete_clean_timing_cannot_be_replaced_by_changing_iterations(self):
        for external in (False, True):
            with self.subTest(external=external):
                old = {"status": "measured", "pairs": 30, "iterations": 50, "ratio": {"upper_95": .9}}
                result, _, timing_calls, _ = self.collect(external, old, {}, iterations=200)
                self.assertEqual(result, 2)
                self.assertEqual(timing_calls, 0)

    def test_diagnostic_timing_can_advance_to_the_required_pair_count(self):
        for external in (False, True):
            with self.subTest(external=external):
                old = {"status": "measured", "pairs": 3, "iterations": 50}
                _, _, timing_calls, _ = self.collect(external, old, {})
                self.assertEqual(timing_calls, 1)

    def test_new_observer_retries_memory_without_resampling_clean_timing(self):
        for external in (False, True):
            with self.subTest(external=external):
                timing = {"status": "measured", "pairs": 30, "iterations": 50,
                          "ratio": {"upper_95": .9}}
                memory = {"status": "observer-unqualified", "raw": "old observer result"}
                _, row, timing_calls, memory_calls = self.collect(
                    external, timing, memory,
                    previous_instrumentation={"probe_sha256": "probe", "observer_sha256": "old-observer"})
                self.assertEqual((timing_calls, memory_calls), (0, 1))
                self.assertEqual(row["timing"], timing)
                self.assertEqual(row["memory"]["status"], "new-memory")
                self.assertEqual(len(row["previous_measurement_report"]["sha256"]), 64)

    def test_complete_memory_cannot_be_replaced_by_changing_pair_count(self):
        for external in (False, True):
            with self.subTest(external=external):
                memory = {"status": "qualified", "pairs": 30, "heap_ratio_upper_95": 1.2}
                result, _, _, memory_calls = self.collect(external, {}, memory, pairs=40)
                self.assertEqual(result, 2)
                self.assertEqual(memory_calls, 0)

    def test_reuse_cannot_overwrite_the_previous_report_or_raw_directory(self):
        for external in (False, True):
            with self.subTest(external=external):
                result, _, timing_calls, memory_calls = self.collect(
                    external, {"status": "contaminated"}, {}, overwrite_previous=True)
                self.assertEqual(result, 2)
                self.assertEqual((timing_calls, memory_calls), (0, 0))


if __name__ == "__main__":
    unittest.main()
