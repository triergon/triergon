"""The owner's initial-delivery ceiling does not replace the optimization goal."""
import copy
import unittest

from .common import load
from .runner import ROOT, acceptance, ratio_interval
from .test_runner import matched_gate_fixture


class DeliveryPolicyTests(unittest.TestCase):
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
