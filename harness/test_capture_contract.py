"""Keep the capture collector's finite rate search tied to owner-approved R2."""
import ast
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parent.parent
APPROVED_RATE_FACTORS = [1.25, 1.6, 2.0, 2.6, 3.4, 4.5]


class CaptureContractTests(unittest.TestCase):
    def test_encoder_rate_loop_matches_owner_approved_contract(self):
        contract = json.loads((ROOT / "harness/capture/contract.json").read_text())
        factors = contract["comparison_rules"].get("encoder_rate_factors")
        tree = ast.parse((ROOT / "harness/capture/compare.py").read_text())
        loops = [
            node
            for node in ast.walk(tree)
            if isinstance(node, ast.For)
            and isinstance(node.target, ast.Name)
            and node.target.id == "factor"
        ]
        self.assertEqual(len(loops), 1, "expected one finite encoder rate-search loop")
        self.assertIsInstance(loops[0].iter, ast.Tuple)
        collector_factors = list(ast.literal_eval(loops[0].iter))
        with self.subTest(check="contract_records_owner_approved_six_points"):
            self.assertEqual(factors, APPROVED_RATE_FACTORS)
        with self.subTest(check="collector_matches_contract"):
            self.assertEqual(collector_factors, factors)


if __name__ == "__main__":
    unittest.main()
