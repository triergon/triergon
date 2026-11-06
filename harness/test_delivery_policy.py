"""The owner's initial-delivery ceiling does not replace the optimization goal."""
import copy
import importlib.util
from pathlib import Path
import shutil
import tempfile
import unittest

from .runner import ROOT, acceptance, load, ratio_interval
from .test_runner import matched_gate_fixture


class DeliveryPolicyTests(unittest.TestCase):
    def test_regeneration_preserves_contract_policy_and_selected_inputs(self):
        for name in ("zlib", "seek"):
            with self.subTest(generator=name), tempfile.TemporaryDirectory() as directory:
                spec = importlib.util.spec_from_file_location(
                    "delivery_generator_" + name, ROOT / "tools" / ("generate-" + name + "-corpus.py")
                )
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                module.ROOT = Path(directory)
                (module.ROOT / "harness").mkdir()
                fixture = Path("research/implementation/cases/avc-aac-sub/reference.json")
                (module.ROOT / fixture).parent.mkdir(parents=True)
                shutil.copy2(ROOT / fixture, module.ROOT / fixture)
                module.main()
                for suffix in ("slice", "corpus"):
                    relative = Path("harness") / (name + "-" + suffix + ".json")
                    self.assertEqual(load(module.ROOT / relative), load(ROOT / relative))

    def test_each_working_slice_keeps_both_explicit_targets(self):
        for prefix in ("", "header-", "zlib-", "seek-", "colour-", "catalogue-"):
            with self.subTest(prefix=prefix):
                contract = load(ROOT / "harness" / (prefix + "slice.json"))
                self.assertEqual(contract["runtime_ratio_max"], 1.2)
                self.assertEqual(contract["runtime_optimization_ratio_target"], .80)
                self.assertEqual(contract["runtime_policy"], "initial-delivery-runtime-v2")

    def test_delivery_tolerance_preserves_raw_work_and_optimization_verdict(self):
        args = matched_gate_fixture()
        contract, rows, _, _, _ = args
        contract["runtime_ratio_max"] = 1.2
        for elapsed, passes in ((119, True), (121, False)):
            for pair in rows[0]["timing"]["samples"]:
                pair["candidate"]["data"]["elapsed_ns"] = elapsed
            rows[0]["timing"]["ratio"] = ratio_interval([(elapsed, 100)] * 30)
            self.assertEqual(acceptance(*args)["slice_accepted"], passes)
        original = copy.deepcopy(args)
        original[0]["runtime_ratio_max"] = .80
        self.assertFalse(acceptance(*original)["slice_accepted"])
        for pair in rows[0]["timing"]["samples"]:
            pair["candidate"]["data"]["elapsed_ns"] = 119
        rows[0]["timing"]["ratio"] = ratio_interval([(119, 100)] * 30)
        self.assertTrue(acceptance(*args)["slice_accepted"])
        rows[0]["parity"]["status"] = "mismatch"
        self.assertFalse(acceptance(*args)["slice_accepted"])
        rows[0]["parity"]["status"] = "pass"
        rows[0]["memory"]["heap_ratio_upper_95"] = 1.0
        self.assertFalse(acceptance(*args)["slice_accepted"])
