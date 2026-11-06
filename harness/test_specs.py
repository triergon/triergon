"""The FATE inventory resolves sample references without executing make."""
import json
from pathlib import Path
import tempfile
import unittest
from .runner import ROOT


class FateInventoryTests(unittest.TestCase):
    def inventory(self, definition):
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'source'
            (source / 'tests/fate').mkdir(parents=True)
            (source / 'tests/fate/authored.mak').write_text(definition)
            output = root / 'inventory.json'
            subprocess.run([sys.executable, str(ROOT / 'tools/inventory_fate.py'),
                            '--source', str(source), '--output', str(output)],
                           check=True, capture_output=True, text=True)
            return json.loads(output.read_text())

    def test_case_local_source_variable_resolves_the_required_external_sample(self):
        report = self.inventory('fate-transport-change: SRC = $(TARGET_SAMPLES)/transport/change.ts\n'
                                'fate-transport-change: CMD = probe -i "$(SRC)"\n')
        case, = report['cases']
        self.assertEqual(case['sample_references'], ['transport/change.ts'])
        self.assertEqual(case['status'], 'unmapped')
        self.assertEqual(case['rights'], 'unreviewed')

    def test_continued_commands_keep_all_literal_samples_once(self):
        report = self.inventory('fate-two-inputs: CMD = compare $(TARGET_SAMPLES)/a/input.mov \\\n'
                                ' $(TARGET_SAMPLES)/b/input.ts $(TARGET_SAMPLES)/a/input.mov\n')
        case, = report['cases']
        self.assertEqual(case['sample_references'], ['a/input.mov', 'b/input.ts'])

    def test_dynamic_sample_names_stay_unresolved_without_executing_make(self):
        report = self.inventory('fate-dynamic: SRC = $(TARGET_SAMPLES)/folder/$(UNEXPANDED).ts\n'
                                'fate-dynamic: CMD = probe -i "$(SRC)" $(shell exit 99)\n')
        case, = report['cases']
        self.assertEqual(case['sample_references'], [])
        self.assertEqual(case['unresolved_sample_references'], ['folder/$(UNEXPANDED).ts'])
        self.assertEqual(case['status'], 'unmapped')
