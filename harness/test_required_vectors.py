import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'required_vectors', Path(__file__).resolve().parents[1] / 'tools/fetch-required-vectors.py')
fetcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetcher)


class RequiredVectorTests(unittest.TestCase):
    def contract(self, root, filename='mkv/required.mkv', content=b'original vector'):
        (root / 'harness').mkdir()
        (root / 'harness/catalogue-slice.json').write_text(json.dumps({
            'required_additional_vectors': [{
                'file': filename, 'sha256': hashlib.sha256(content).hexdigest()}]}))

    def test_cached_original_is_verified_without_network(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract(root)
            path = root / '.research/fate-samples/mkv/required.mkv'
            path.parent.mkdir(parents=True)
            path.write_bytes(b'original vector')
            with patch.object(fetcher.urllib.request, 'urlopen', side_effect=AssertionError('no fetch')):
                self.assertEqual(fetcher.fetch_required(root), ['mkv/required.mkv'])

    def test_changed_cache_is_preserved_and_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract(root)
            path = root / '.research/fate-samples/mkv/required.mkv'
            path.parent.mkdir(parents=True)
            path.write_bytes(b'changed')
            with self.assertRaisesRegex(ValueError, 'cached required vector changed'):
                fetcher.fetch_required(root)
            self.assertEqual(path.read_bytes(), b'changed')

    def test_wrong_download_is_not_published_as_a_cached_vector(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract(root)
            with patch.object(fetcher.urllib.request, 'urlopen', return_value=io.BytesIO(b'wrong')):
                with self.assertRaisesRegex(ValueError, 'download hash mismatch'):
                    fetcher.fetch_required(root)
            self.assertFalse((root / '.research/fate-samples/mkv/required.mkv').exists())

    def test_contract_path_cannot_escape_the_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.contract(root, '../outside.mkv')
            with self.assertRaisesRegex(ValueError, 'invalid required vector path'):
                fetcher.fetch_required(root)
