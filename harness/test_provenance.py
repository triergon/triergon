import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from .provenance import matroska_inputs, selected_graph


class SelectedGraphTests(unittest.TestCase):
    def test_measurement_reuse_policy_is_a_pinned_operation_input(self):
        root = Path(__file__).resolve().parents[1]
        self.assertIn('harness/reuse.py', matroska_inputs(root))

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers=["core","worker","other"]\n[workspace.package]\nedition="2024"\n[profile.release]\nlto="thin"\n')
        self.packages = []
        for name in ('core', 'worker', 'other'):
            path = self.root / name
            (path / 'src' / 'nested').mkdir(parents=True)
            (path / 'Cargo.toml').write_text('[package]\nname="' + name + '"\nversion="0.0.0"\n')
            (path / 'src' / 'lib.rs').write_text('// original ' + name)
            (path / 'src' / 'nested' / 'syntax.rs').write_text('// nested ' + name)
            self.packages.append({'id': name, 'name': name, 'version': '0.0.0', 'source': None,
                                  'manifest_path': str(path / 'Cargo.toml'), 'targets': []})
        self.metadata = {'packages': self.packages, 'resolve': {'nodes': [
            {'id': 'worker', 'features': [], 'deps': [{'pkg': 'core', 'name': 'core', 'dep_kinds': [{'kind': None, 'target': None}]}]},
            {'id': 'core', 'features': [], 'deps': []},
            {'id': 'other', 'features': [], 'deps': []},
        ]}}

    def capture(self):
        return selected_graph(self.root, self.metadata, 'worker', ['harness-rule.json'])

    def test_unrelated_code_and_workspace_members_do_not_change_selected_graph(self):
        (self.root / 'harness-rule.json').write_text('{"gate":0.8}')
        before = self.capture()
        (self.root / 'other/src/lib.rs').write_text('// unrelated edit')
        manifest = self.root / 'Cargo.toml'
        manifest.write_text(manifest.read_text().replace('"other"', '"other","new-lane"'))
        self.assertEqual(before, self.capture())
        self.assertEqual([p['name'] for p in before['packages']], ['core', 'worker'])

    def test_finder_metadata_is_preserved_but_is_not_a_build_input(self):
        (self.root / 'harness-rule.json').write_text('{}')
        before = self.capture()
        metadata = self.root / 'core/.DS_Store'
        metadata.write_bytes(b'Finder state')
        self.assertEqual(before, self.capture())
        metadata.write_bytes(b'another Finder state')
        self.assertEqual(before, self.capture())
        self.assertTrue(metadata.is_file())

    def test_nested_sources_assets_rules_and_shared_profiles_are_part_of_identity(self):
        rule = self.root / 'harness-rule.json'
        rule.write_text('{"gate":0.8}')
        previous = self.capture()['sha256']
        for path in (self.root / 'core/src/nested/syntax.rs', self.root / 'worker/src/asset.bin', rule):
            path.write_bytes(b'changed')
            current = self.capture()['sha256']
            self.assertNotEqual(previous, current)
            previous = current
        manifest = self.root / 'Cargo.toml'
        manifest.write_text(manifest.read_text().replace('lto="thin"', 'lto="fat"'))
        self.assertNotEqual(previous, self.capture()['sha256'])

    def test_added_dependency_features_and_manifest_changes_invalidate(self):
        (self.root / 'harness-rule.json').write_text('{}')
        before = self.capture()['sha256']
        self.metadata['resolve']['nodes'][0]['features'] = ['extra']
        self.assertNotEqual(before, self.capture()['sha256'])
        before = self.capture()['sha256']
        self.metadata['resolve']['nodes'][0]['deps'].append({'pkg': 'other', 'name': 'other', 'dep_kinds': []})
        self.assertNotEqual(before, self.capture()['sha256'])
        before = self.capture()['sha256']
        (self.root / 'core/Cargo.toml').write_text('[package]\nname="core"\nversion="0.1.0"\n')
        self.assertNotEqual(before, self.capture()['sha256'])

    def test_missing_or_external_sources_and_symlinks_fail_closed(self):
        (self.root / 'harness-rule.json').write_text('{}')
        self.packages[0]['source'] = 'registry+unadmitted'
        with self.assertRaises(ValueError):
            self.capture()
        self.packages[0]['source'] = None
        source = self.root / 'core/src/nested/syntax.rs'
        source.unlink()
        source.symlink_to('/dev/null')
        with self.assertRaises(ValueError):
            self.capture()

    def test_file_additions_deletions_and_missing_dependency_nodes_are_visible(self):
        (self.root / 'harness-rule.json').write_text('{}')
        before = self.capture()['sha256']
        path = self.root / 'core/src/another.rs'
        path.write_text('// new module')
        self.assertNotEqual(before, self.capture()['sha256'])
        path.unlink()
        self.assertEqual(before, self.capture()['sha256'])
        self.metadata['resolve']['nodes'] = self.metadata['resolve']['nodes'][:1]
        with self.assertRaises(ValueError):
            self.capture()

    def test_collector_verifier_rejects_source_drift_in_its_selected_graph(self):
        source = Path(__file__).resolve().parents[1] / 'tools/test-media.py'
        spec = importlib.util.spec_from_file_location('test_media_collector', source)
        collector = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(collector)
        self.packages[1]['name'] = 'media-worker'
        (self.root / 'harness-rule.json').write_text('{}')
        graph = selected_graph(self.root, self.metadata, 'media-worker', ['harness-rule.json'])
        identity = {'schema': 2, 'selected_graph': graph}
        with patch.object(collector, 'ROOT', self.root), patch('harness.provenance.cargo_metadata', return_value=self.metadata), patch('harness.provenance.matroska_inputs', return_value=['harness-rule.json']):
            self.assertIsNone(collector.source_identity_error(identity))
            (self.root / 'other/src/nested/syntax.rs').write_text('// unrelated')
            self.assertIsNone(collector.source_identity_error(identity))
            (self.root / 'core/src/nested/syntax.rs').write_text('// selected source moved')
            self.assertIsNotNone(collector.source_identity_error(identity))
            self.assertIsNotNone(collector.source_identity_error({'schema': 999}))
