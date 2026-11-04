"""Regression coverage for the owner's explicit resident-memory policy choice."""
import copy
import unittest

from .delivery_memory_v2 import POLICY, ResidentPeakPair, total_resident_rss_gate
from .runner import ratio_interval


class TotalResidentDeliveryTests(unittest.TestCase):
    def test_lower_total_can_pass_while_old_operation_growth_fails(self):
        candidate_start, candidate_peak = 2_166_784, 2_625_536
        reference_start, reference_peak = 2_900_992, 3_105_792
        pairs = [ResidentPeakPair(candidate_peak, reference_peak, 4096)] * 30
        before = copy.deepcopy(pairs)
        old = ratio_interval([(candidate_peak - candidate_start + 8192,
                               reference_peak - reference_start - 8192)] * 30)
        new = total_resident_rss_gate(pairs)
        self.assertGreater(old["upper_95"], 1)
        self.assertTrue(new["lower_total_resident_RSS"])
        self.assertEqual(new["policy"], POLICY)
        self.assertEqual(pairs, before)

    def test_strict_boundary_and_two_page_allowance_are_not_removed(self):
        pair = ResidentPeakPair(100_000, 116_384, 4096)
        result = total_resident_rss_gate([pair] * 30)
        self.assertEqual(result["bounded_peak_pairs"], [(108_192, 108_192)] * 30)
        self.assertEqual(result["ratio"]["upper_95"], 1)
        self.assertFalse(result["lower_total_resident_RSS"])
        self.assertLess(pair.candidate_upper_bytes / pair.reference_observed_bytes, 1)

    def test_uses_paired_confidence_bound_instead_of_only_the_median(self):
        pairs = [ResidentPeakPair(80_000, 116_384, 4096)] * 16
        pairs += [ResidentPeakPair(140_000, 116_384, 4096)] * 14
        result = total_resident_rss_gate(pairs)
        self.assertLess(result["ratio"]["median"], 1)
        self.assertGreater(result["ratio"]["upper_95"], 1)
        self.assertFalse(result["lower_total_resident_RSS"])

    def test_missing_pairs_and_nonresident_or_invalid_values_cannot_pass(self):
        good = ResidentPeakPair(80_000, 120_000, 4096)
        for pairs in ([good] * 29, [good] * 31, [None] * 30):
            with self.subTest(pairs=len(pairs)), self.assertRaises(ValueError):
                total_resident_rss_gate(pairs)
        for pair in (ResidentPeakPair(True, 120_000, 4096),
                     ResidentPeakPair(-1, 120_000, 4096),
                     ResidentPeakPair(0, 120_000, 4096),
                     ResidentPeakPair(80_000, 8192, 4096),
                     ResidentPeakPair(80_000, 120_000, 3000),
                     ResidentPeakPair(80_000.0, 120_000, 4096)):
            with self.subTest(pair=pair), self.assertRaises(ValueError):
                total_resident_rss_gate([pair] * 30)


if __name__ == "__main__":
    unittest.main()
