#!/usr/bin/env python3
"""Exercise the CI scope and target cache helpers in isolated Git checkouts."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    'ci_scope', Path(__file__).resolve().parent / 'ci-scope.py')
ci_scope = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci_scope)

BASE_FILES = {
    'README.md': 'workspace\n',
    'docs/status.md': 'status\n',
    'harness/evidence/sets/run.json': '{}\n',
    'harness/evidence/index.json': '{}\n',
    'harness/features/seek/README.md': 'feature\n',
    'crates/a/README.md': 'crate\n',
    'crates/a/NOTES.md': 'notes\n',
    'crates/a/src/lib.rs': '#![doc = include_str!("../README.md")]\n',
    'crates/a/tests/fixtures/README.md': 'fixtures\n',
    'crates/b/tests/cases/README.md': 'cases\n',
    'xtask/src/main.rs': 'fn main() {}\n',
}


class GitFixture(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'test')
        self.git('config', 'user.email', 'test@example.invalid')
        for path, text in BASE_FILES.items():
            self.write(path, text)
        self.base = self.commit('base')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root).decode().strip()

    def write(self, path, text):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def commit(self, message):
        self.git('add', '-A')
        self.git('commit', '-q', '--allow-empty', '-m', message)
        return self.git('rev-parse', 'HEAD')


class Scope(GitFixture):
    def scope(self, *edits):
        """The scope of EDITS made on top of the base commit."""
        self.git('reset', '-q', '--hard', self.base)
        for edit in edits:
            edit()
        head = self.commit('change')
        cwd = os.getcwd()
        os.chdir(self.root)
        try:
            return ci_scope.scope(self.base, head)
        finally:
            os.chdir(cwd)

    def edit(self, path, text='edited\n'):
        return lambda: self.write(path, text)

    def remove(self, path):
        return lambda: (self.root / path).unlink()

    def test_documents_and_evidence_sets_are_documentation(self):
        self.assertEqual(self.scope(self.edit('docs/status.md')), 'docs')
        self.assertEqual(self.scope(self.edit('docs/new.md')), 'docs')
        self.assertEqual(self.scope(self.edit('harness/evidence/sets/new.json')), 'docs')
        self.assertEqual(self.scope(self.remove('harness/evidence/sets/run.json')), 'docs')

    def test_markdown_edited_in_place_is_documentation(self):
        self.assertEqual(
            self.scope(self.edit('README.md'), self.edit('crates/a/NOTES.md'),
                       self.edit('docs/status.md')),
            'docs')

    def test_included_markdown_runs_everything(self):
        self.assertEqual(self.scope(self.edit('crates/a/README.md')), 'full')

    def test_markdown_beside_test_inputs_runs_everything(self):
        for path in ['harness/features/seek/README.md', 'crates/a/tests/fixtures/README.md',
                     'crates/b/tests/cases/README.md']:
            with self.subTest(path=path):
                self.assertEqual(self.scope(self.edit(path, path)), 'full')

    def test_added_removed_or_renamed_markdown_runs_everything(self):
        self.assertEqual(self.scope(self.edit('crates/a/NEW.md')), 'full')
        self.assertEqual(self.scope(self.remove('crates/a/NOTES.md')), 'full')
        self.assertEqual(
            self.scope(lambda: self.git('mv', 'README.md', 'GUIDE.md')), 'full')

    def test_any_other_file_runs_everything(self):
        for path in ['xtask/src/main.rs', 'harness/evidence/index.json',
                     'crates/a/notes.txt', 'docs.md/x.rs']:
            with self.subTest(path=path):
                self.assertEqual(
                    self.scope(self.edit('docs/status.md', path), self.edit(path, path)),
                    'full')

    def test_an_empty_change_runs_everything(self):
        self.assertEqual(self.scope(), 'full')

    def test_a_symbolic_link_runs_everything(self):
        self.assertEqual(
            self.scope(lambda: os.symlink('../xtask/src/main.rs', self.root / 'docs/link.md')),
            'full')

    def test_an_include_built_from_parts_counts_for_the_names_its_source_mentions(self):
        self.write('crates/a/src/lib.rs',
                   'macro_rules! read { ($path:literal) => { include_str!($path) }; }\n'
                   'const A: &str = read!("../NOTES.md");\n')
        self.base = self.commit('include through a macro')
        self.assertEqual(self.scope(self.edit('crates/a/NOTES.md')), 'full')
        self.assertEqual(self.scope(self.edit('README.md')), 'docs')

    def test_an_include_built_from_parts_with_another_extension_is_no_markdown(self):
        self.write('crates/a/src/lib.rs',
                   'const A: &[u8] = include_bytes!(concat!(\n'
                   '    env!("CARGO_MANIFEST_DIR"),\n    "/NOTES.md.ts"\n));\n')
        self.base = self.commit('include a stream')
        self.assertEqual(self.scope(self.edit('crates/a/NOTES.md')), 'docs')

    def test_an_include_split_over_lines_is_resolved(self):
        self.write('crates/a/src/lib.rs', 'const A: &str = include_str!(\n    "../NOTES.md"\n);\n')
        self.base = self.commit('split include')
        self.assertEqual(self.scope(self.edit('crates/a/NOTES.md')), 'full')
        self.assertEqual(self.scope(self.edit('README.md')), 'docs')


class TargetCache(GitFixture):
    helper = Path(__file__).resolve().with_name('target-cache.py')
    unchanged = 946684800

    def cache(self, operation, check=True):
        return subprocess.run([sys.executable, self.helper, operation, 'target'],
                              cwd=self.root, capture_output=True, check=check)

    def test_dirty_record_preserves_previous_cache_index(self):
        self.cache('record')
        index = self.root / 'target/.sources'
        previous = index.read_bytes()
        self.write('README.md', 'dirty')
        self.assertNotEqual(self.cache('record', check=False).returncode, 0)
        self.assertEqual(index.read_bytes(), previous)

    def test_restored_target_without_record_is_discarded(self):
        self.write('target/old', 'stale artifact')
        self.cache('apply')
        self.assertFalse((self.root / 'target').exists())
        self.cache('apply')

    def test_apply_compares_modes_and_contents_and_dates_links_separately(self):
        self.git('config', 'core.filemode', 'true')
        link = self.root / 'link'
        link.symlink_to('README.md')
        self.commit('add link')
        self.cache('record')
        edited = self.root / 'README.md'
        executable = self.root / 'xtask/src/main.rs'
        edited.write_text('new contents')
        executable.chmod(0o755)
        self.git('add', 'README.md', 'xtask/src/main.rs')
        for path in (edited, executable):
            os.utime(path, (self.unchanged, self.unchanged))
        self.cache('apply')
        self.assertEqual(link.lstat().st_mtime, self.unchanged)
        self.assertEqual((self.root / 'docs/status.md').stat().st_mtime, self.unchanged)
        self.assertGreater(edited.stat().st_mtime, self.unchanged)
        self.assertGreater(executable.stat().st_mtime, self.unchanged)
        self.assertFalse((self.root / 'target/.sources').exists())


if __name__ == '__main__':
    unittest.main()
