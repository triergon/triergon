"""Reproduction must verify every frozen normative source, including zlib RFCs."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from .runner import ROOT, load

spec = importlib.util.spec_from_file_location('spec_fetcher', ROOT / 'tools/fetch-specs.py')
fetcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetcher)


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
                    rows.append({**source, 'sha256': hashlib.sha256(data).hexdigest()})
                (root / 'specs/sources.json').write_text(json.dumps({'sources': rows}))
                (cache / damaged).write_bytes(b'corrupt')
                with patch.object(fetcher, 'ROOT', root), patch('sys.argv', ['fetch-specs.py']), patch('urllib.request.urlopen', side_effect=AssertionError('cached verification must not fetch')), contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaisesRegex(ValueError, damaged):
                        fetcher.main()
