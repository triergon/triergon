import unittest
from unittest.mock import patch
from .runner import acceptance, differences, ratio_interval, ROOT, load


class HarnessTests(unittest.TestCase):
    def test_empty_or_missing_evidence_cannot_pass(self):
        contract = load(ROOT / "harness/slice.json")
        result = acceptance(contract, [], {}, "aarch64-apple-darwin", [])
        self.assertFalse(result["accepted"])
        self.assertTrue(result["missing_profiles"])

    def test_payload_time_key_and_unknowns_are_not_normalized_away(self):
        left = {"payload": "01", "pts_ns": -1, "dts_ns": None, "key": False}
        right = {"payload": "02", "pts_ns": 0, "dts_ns": 0, "key": True}
        self.assertEqual(len(differences(left, right)), 4)
        self.assertTrue(differences({"a": False}, {"a": 0}))
        self.assertTrue(differences([1], []))

    def test_paired_confidence_and_invalid_samples(self):
        interval = ratio_interval([(79, 100)] * 30)
        self.assertAlmostEqual(interval["upper_95"], .79)
        self.assertGreater(ratio_interval([(81, 100)] * 30)["upper_95"], .8)
        for pairs in ([], [(0, 1)] * 2, [(1, 0)] * 2, [(float("nan"), 1)] * 2):
            with self.assertRaises(ValueError):
                ratio_interval(pairs)

    def test_fast_run_does_not_override_correctness_or_memory(self):
        contract = load(ROOT / "harness/slice.json")
        rows = [{"id": "fast", "profile": "avc-aac-sub", "parity": {"status": "mismatch"},
                 "timing": {"status": "measured", "pairs": 30, "ratio": {"upper_95": .1}, "contaminants": []}}]
        self.assertFalse(acceptance(contract, rows, {}, "aarch64-apple-darwin", [])["accepted"])

    def test_competing_builds_prevent_timing_workers_from_starting(self):
        from .runner import benchmark
        with patch("harness.runner.competing_builds", return_value=["rustc"]), patch("harness.runner.execute") as worker:
            result = benchmark("candidate", "reference", "fixture", 30, 200, 0)
        self.assertEqual(result["status"], "contaminated")
        worker.assert_not_called()

    def test_idle_process_names_are_not_active_work_but_any_cpu_advance_is(self):
        from .runner import active_cpu
        before = {1:{"name":"ffmpeg","cpu_ticks":100,"state":"S"}}
        self.assertEqual(active_cpu(before, before), [])
        self.assertEqual(active_cpu(before, {1:{"name":"ffmpeg","cpu_ticks":101,"state":"S"}}), ["ffmpeg"])
        self.assertEqual(active_cpu(before, {1:{"name":"ffmpeg","cpu_ticks":100,"state":"R"}}), ["ffmpeg"])

    def test_non_finite_or_incomplete_measurements_never_satisfy_a_gate(self):
        contract = load(ROOT / "harness/slice.json")
        for ratio in (float("nan"), float("inf"), 0, -1):
            row = {"id": "bad", "profile": "avc-aac-sub", "parity": {"status": "pass"},
                   "timing": {"status": "measured", "pairs": 30, "ratio": {"upper_95": ratio}},
                   "memory": {"status": "qualified", "rss_ratio_upper_95": ratio, "heap_ratio_upper_95": ratio}}
            result = acceptance(contract, [row], {"source_revision": "pin"}, "aarch64-apple-darwin", [{"status": "pass"}])
            self.assertTrue(any("runtime gate" in r for r in result["reasons"]))
            self.assertTrue(any("allocation gate" in r for r in result["reasons"]))

    def test_complete_matched_evidence_can_pass_and_summary_tampering_fails(self):
        from .memory import evaluate
        contract = {"required_profiles": ["fixture"], "required_targets": ["host"], "minimum_pairs": 30, "runtime_ratio_max": .8, "pending_module_requirements": []}
        def result(ns):
            return {"status": "ok", "data": {"elapsed_ns": ns, "iterations": 100, "packets": 10, "bytes": 100, "digest": 9}}
        pairs = [{"candidate": result(79), "reference": result(100), "cpu_before":{}, "cpu_after":{}} for _ in range(30)]
        hashes={"candidate":"candidate-hash","reference":"reference-hash"}
        certificate={"status":"qualified-selected-workers","errors":[],"worker_hashes":hashes,"observer_sha256":"observer-hash",
                     "probe_sha256":"probe-hash", "probe_heap":{"status":"measured","observer":{"heap_peak_bytes":100,"heap_live_start":0,"heap_live_end":0},"work":{"expected_peak_bytes":100}}}
        memory_samples=[]
        for _ in range(30):
            sample={}
            for name in ("candidate","reference"):
                for mode in ("heap","rss"):
                    sample[name+"_"+mode]={"status":"measured","instrument_sha256":"observer-hash","work":{"packets":10},
                        "observer":{"status":"measured","heap_peak_bytes":79 if name=="candidate" else 100,
                                    "rss_start":100,"rss_peak_observed":160 if name=="candidate" else 300,"hwm_end":160 if name=="candidate" else 300,"page_size":1}}
            memory_samples.append(sample)
        qualified_memory=evaluate(memory_samples,30,0,certificate,ratio_interval)
        row = {"id": "fixture", "profile": "fixture", "parity": {"status": "pass", "source_timing_status": "pass"},
               "timing": {"status": "measured", "worker_hashes": hashes, "samples": pairs, "pairs": 30, "seed": 0, "ratio": ratio_interval([(79,100)]*30)},
               "memory": qualified_memory}
        args = (contract, [row], {"source_revision": "pin","binaries":hashes,"instrumentation":{"probe_sha256":"probe-hash","observer_sha256":"observer-hash"}}, "host", [{"status": "pass"}])
        self.assertTrue(acceptance(*args)["accepted"])
        args[2]["instrumentation"]["probe_sha256"] = "rebuilt-probe"
        self.assertFalse(acceptance(*args)["slice_accepted"])
        args[2]["instrumentation"]["probe_sha256"] = "probe-hash"
        row["timing"]["worker_hashes"] = {"candidate": "candidate-hash", "reference": "a-different-reference"}
        self.assertFalse(acceptance(*args)["slice_accepted"])
        row["timing"]["worker_hashes"] = hashes
        contract["pending_module_requirements"]=["future module work"]
        self.assertFalse(acceptance(*args)["accepted"])
        self.assertTrue(acceptance(*args)["slice_accepted"])
        contract["pending_module_requirements"]=[]
        row["timing"]["ratio"]["upper_95"] = .1
        self.assertFalse(acceptance(*args)["accepted"])
        row["timing"]["ratio"]=ratio_interval([(79,100)]*30)
        row["memory"]["heap_ratio_upper_95"]=.1
        self.assertFalse(acceptance(*args)["slice_accepted"])


if __name__ == "__main__":
    unittest.main()

class MemoryCacheTests(unittest.TestCase):
    def test_rebuilt_probe_or_observer_prevents_memory_reuse(self):
        from .memory import reusable_memory
        measurement = {'status': 'qualified', 'pairs': 30,
                       'certificate': {'probe_sha256': 'probe', 'observer_sha256': 'observer'}}
        current = {'probe_sha256': 'probe', 'observer_sha256': 'observer'}
        self.assertTrue(reusable_memory(measurement, 30, current))
        for field in current:
            changed = {**current, field: 'rebuilt'}
            self.assertFalse(reusable_memory(measurement, 30, changed))
        self.assertFalse(reusable_memory(measurement, 31, current))
