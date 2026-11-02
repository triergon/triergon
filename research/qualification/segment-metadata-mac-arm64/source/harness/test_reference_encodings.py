"""Native missing-feature supplement: full source facts and explicit capability bounds."""
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from .runner import ROOT
from .spec_oracle import elements
from .content_oracle import content_trace, projected_content

spec = importlib.util.spec_from_file_location('reference_fixture', ROOT / 'tools/generate-zlib-corpus.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ReferenceEncodingTests(unittest.TestCase):
    def test_deflate_lacing_empty_and_encoding_storage_order_against_source_oracle(self):
        reference = ROOT / 'target/harness/ffmpeg-combined-worker'
        candidate = ROOT / 'target/release/media-worker'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'mixed.mkv'
            for kind in ['stored', 'fixed', 'dynamic']:
                for lace in range(4):
                    for empty in [False, True]:
                        data = fixture.make(kind=kind, lace=lace, empty=empty, mixed=True)
                        _, a, b, _ = next(e for e in elements(data, 0, len(data)) if e[0] == 0x18538067)
                        _, a, b, _ = next(e for e in elements(data, a, b) if e[0] == 0x1654ae6b)
                        _, a, b, _ = next(e for e in elements(data, a, b) if e[0] == 0xae)
                        _, a, b, _ = next(e for e in elements(data, a, b) if e[0] == 0x6d80)
                        stages = [fixture.e(tag, data[x:y]) for tag, x, y, _ in elements(data, a, b)]
                        for reverse in [False, True]:
                            alternate = fixture.e(0x6d80, b''.join(reversed(stages) if reverse else stages))
                            source = data.replace(fixture.e(0x6d80, b''.join(stages)), alternate)
                            self.assertEqual(len(source), len(data))
                            path.write_bytes(source)
                            expected = content_trace(path)
                            native = json.loads(subprocess.check_output([reference, 'trace', path], stderr=subprocess.PIPE))
                            rust = json.loads(subprocess.check_output([candidate, 'trace', path], stderr=subprocess.PIPE))
                            self.assertEqual(projected_content(native), expected)
                            self.assertEqual(projected_content(rust), expected)
                            self.assertEqual(native, rust)

    def test_combined_private_scope_is_an_explicit_unsupported_reference_case(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'private.mkv'
            path.write_bytes(fixture.make(mixed=True, scope=3))
            result = subprocess.run([ROOT / 'target/harness/ffmpeg-combined-worker', 'trace', path], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
