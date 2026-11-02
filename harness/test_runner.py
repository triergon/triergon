import unittest
from unittest.mock import patch
from .runner import acceptance, differences, ratio_interval, ROOT, load


def matched_gate_fixture():
    from .memory import evaluate
    contract={"required_profiles":["fixture"],"required_targets":["host"],"minimum_pairs":30,
              "runtime_ratio_max":.8,"pending_module_requirements":[]}
    def result(ns):
        return {"status":"ok","data":{"elapsed_ns":ns,"iterations":100,"packets":10,"bytes":100,"digest":9}}
    pairs=[{"pair":index,"candidate":result(79),"reference":result(100),"cpu_before":{},"cpu_after":{}}
           for index in range(30)]
    hashes={"candidate":"candidate-hash","reference":"reference-hash"}
    certificate={"status":"qualified-selected-workers","errors":[],"worker_hashes":hashes,"observer_sha256":"observer-hash",
                 "probe_sha256":"probe-hash","probe_heap":{"status":"measured", "observer":{"heap_peak_bytes":100,"heap_live_start":0,"heap_live_end":0},"work":{"expected_peak_bytes":100}}}
    samples=[]
    for _ in range(30):
        sample={}
        for name in ("candidate","reference"):
            for mode in ("heap","rss"):
                sample[name+"_"+mode]={"status":"measured","instrument_sha256":"observer-hash","work":{"packets":10},
                    "observer":{"status":"measured","heap_peak_bytes":79 if name=="candidate" else 100,
                                "rss_start":100,"rss_peak_observed":160 if name=="candidate" else 300,
                                "hwm_end":160 if name=="candidate" else 300,"page_size":1}}
        samples.append(sample)
    row={"id":"fixture","profile":"fixture","parity":{"status":"pass","source_timing_status":"pass"},
         "timing":{"status":"measured","worker_hashes":hashes,"samples":pairs,"pairs":30,"iterations":100,
                   "seed":0,"ratio":ratio_interval([(79,100)]*30)},
         "memory":evaluate(samples,30,0,certificate,ratio_interval)}
    identity={"source_revision":"pin","binaries":hashes,
              "instrumentation":{"probe_sha256":"probe-hash","observer_sha256":"observer-hash"}}
    return contract,[row],identity,"host",[{"status":"pass"}]


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
        with patch("harness.runner.timing_module_state", return_value={"status": "module-absent"}), \
                patch("harness.runner.competing_builds", return_value=["rustc"]), \
                patch("harness.runner.execute") as worker:
            result = benchmark("candidate", "reference", "fixture", 30, 200, 0)
        self.assertEqual(result["status"], "contaminated")
        worker.assert_not_called()

    def test_competing_builds_prevent_memory_workers_from_starting(self):
        from .runner import memory
        with patch("harness.runner.competing_builds", return_value=["rustc"]), \
                patch("harness.memory.qualification") as certificate, \
                patch("harness.memory.measure") as worker:
            result = memory("candidate", "reference", "fixture", 30, 0)
        self.assertEqual(result["status"], "contaminated")
        self.assertEqual(result["contaminants"], ["rustc"])
        certificate.assert_not_called()
        worker.assert_not_called()

    def test_cpu_advance_invalidates_memory_but_preserves_raw_observations(self):
        from .runner import memory
        before = {1: {"name": "ffmpeg", "cpu_ticks": 100, "state": "S"}}
        after = {1: {"name": "ffmpeg", "cpu_ticks": 101, "state": "S"}}
        measured = {"status": "qualified", "samples": [{"raw": "retained"}],
                    "rss_ratio_upper_95": .5, "heap_ratio_upper_95": .5}
        with patch("harness.runner.competing_builds", return_value=[]), \
                patch("harness.runner.cpu_snapshot", side_effect=[before, after]), \
                patch("harness.memory.qualification", return_value={"status": "qualified-selected-workers"}), \
                patch("harness.memory.measure", return_value=measured):
            result = memory("candidate", "reference", "fixture", 30, 0)
        self.assertEqual(result["status"], "contaminated")
        self.assertEqual(result["samples"], measured["samples"])
        self.assertEqual(result["measurement_status"], "qualified")
        self.assertEqual(result["cpu_before"], before)
        self.assertEqual(result["cpu_after"], after)
        self.assertEqual(result["contaminants"], ["ffmpeg"])

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
        args=matched_gate_fixture()
        contract,rows,identity,_,_=args
        row,hashes=rows[0],identity['binaries']
        self.assertTrue(acceptance(*args)["accepted"])
        args[2]["schema"] = 2
        self.assertFalse(acceptance(*args)["slice_accepted"])
        row["memory"].update(cpu_before={}, cpu_after={}, contaminants=[])
        self.assertTrue(acceptance(*args)["slice_accepted"])
        row["memory"]["cpu_after"] = {1: {"name": "rustc", "cpu_ticks": 1, "state": "R"}}
        self.assertFalse(acceptance(*args)["slice_accepted"])
        row["memory"]["cpu_after"] = {}
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


class GatePairBindingTests(unittest.TestCase):
    def test_duplicate_missing_or_noninteger_pair_identity_cannot_count_as_thirty_pairs(self):
        import copy
        args=matched_gate_fixture()
        self.assertTrue(acceptance(*args)['slice_accepted'])
        for value in (None,0,False,0.0):
            changed=copy.deepcopy(args)
            for pair in changed[1][0]['timing']['samples']:
                if value is None:pair.pop('pair',None)
                else:pair['pair']=value
            self.assertFalse(acceptance(*changed)['slice_accepted'])

    def test_raw_iterations_must_match_the_declared_complete_operation_count(self):
        args=matched_gate_fixture()
        self.assertTrue(acceptance(*args)['slice_accepted'])
        for pair in args[1][0]['timing']['samples']:
            for role in ('candidate','reference'):pair[role]['data']['iterations']=1
        self.assertFalse(acceptance(*args)['slice_accepted'])

    def test_missing_zero_or_untyped_iteration_count_is_not_measured_work(self):
        import copy
        args=matched_gate_fixture()
        self.assertTrue(acceptance(*args)['slice_accepted'])
        for value in (None,0,True,100.0):
            changed=copy.deepcopy(args)
            if value is None:changed[1][0]['timing'].pop('iterations')
            else:changed[1][0]['timing']['iterations']=value
            self.assertFalse(acceptance(*changed)['slice_accepted'])


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


class CheckpointTests(unittest.TestCase):
    def test_interrupted_replay_keeps_unvisited_clean_passes_and_failures(self):
        from .runner import checkpoint_report, measurement_cache
        failure = {'id': 'later-failure', 'timing': {'status': 'measured', 'ratio': {'upper_95': .85}, 'samples': ['original-failure']}}
        passed = {'id': 'later-pass', 'timing': {'status': 'measured', 'ratio': {'upper_95': .7}, 'samples': ['original-pass']}}
        old = {'cases': [failure, passed]}
        early = {'id': 'early', 'timing': {'status': 'contaminated'}}
        first = checkpoint_report({'identity': 'same-workers'}, [early], measurement_cache(old))
        self.assertEqual(first['cases'], [early])
        self.assertFalse(first['acceptance']['slice_accepted'])
        resumed = measurement_cache(first)
        self.assertEqual(resumed['later-failure'], failure)
        self.assertEqual(resumed['later-pass'], passed)
        second = checkpoint_report({'identity': 'same-workers'}, [early, passed], resumed)
        final_cache = measurement_cache(second)
        self.assertEqual(final_cache['later-failure']['timing']['samples'], ['original-failure'])
        self.assertEqual(final_cache['later-pass']['timing']['samples'], ['original-pass'])
        self.assertEqual(len(final_cache), 3)

    def test_ambiguous_cached_case_identity_fails_closed(self):
        from .runner import measurement_cache
        with self.assertRaises(ValueError):
            measurement_cache({'cases': [{'id': 'same'}], 'cached_cases': [{'id': 'same'}]})
