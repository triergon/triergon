"""Reproduction must verify every frozen normative source, including zlib RFCs."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from tools import fetch_specs as fetcher
from .common import digest_bytes, load
from .runner import ROOT


class SpecificationTests(unittest.TestCase):
    def test_fetch_registry_covers_every_frozen_source(self):
        expected = {source['id'] for source in load(ROOT / 'specs/sources.json')['sources']}
        self.assertEqual({entry[0] for entry in fetcher.SOURCES}, expected)

    def test_both_compression_specifications_are_verified_for_drift(self):
        for damaged in ['rfc1950', 'rfc1951']:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                cache = root / '.research/specs'
                cache.mkdir(parents=True)
                (root / 'specs').mkdir()
                rows = []
                for source in load(ROOT / 'specs/sources.json')['sources']:
                    data = source['id'].encode()
                    (cache / source['id']).write_bytes(data)
                    rows.append({**source, 'sha256': digest_bytes(data)})
                (root / 'specs/sources.json').write_text(json.dumps({'sources': rows}))
                (cache / damaged).write_bytes(b'corrupt')
                with patch.object(fetcher, 'ROOT', root), patch('sys.argv', ['fetch_specs.py']), patch('urllib.request.urlopen', side_effect=AssertionError('cached verification must not fetch')), contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaisesRegex(ValueError, damaged):
                        fetcher.main()


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
