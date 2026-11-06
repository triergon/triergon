import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools import run_fate as fate


# Exercise FATE dispatch with isolated fixtures; no upstream result is fabricated.
import contextlib
import sys

class FateDispatchTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        cache=self.root/'.research/reference'
        cache.mkdir(parents=True)
        (cache/'build.json').write_text(json.dumps({'revision':'frozen-fixture'}))
        (cache/'fate-list.txt').write_text('fate-matroska-xiph-lacing\nfate-matroska-opus-remux\n')
        ffmpeg=cache/'FFmpeg-frozen-fixture/ffmpeg'
        ffmpeg.parent.mkdir()
        ffmpeg.write_bytes(b'unit-fixture-only-oracle')
        samples=self.root/'.research/fate-samples/mkv'
        samples.mkdir(parents=True)
        for n in ['xiph_lacing.mka','codec_delay_opus.mkv']:
            (samples/n).write_bytes(b'unit-fixture-only-input')
        self.addCleanup(patch.stopall)
        patch.object(fate,'ROOT',self.root).start()
        patch.object(fate,'source_identity_error',return_value=None).start()

    def adapters(self):
        candidate=self.root/'target/release/media-worker'
        reference=self.root/'target/harness/ffmpeg-worker'
        for p,b in [(candidate,b'unit-fixture-candidate'),(reference,b'unit-fixture-reference')]:
            p.parent.mkdir(parents=True,exist_ok=True)
            p.write_bytes(b)
        ident={'binaries':{'candidate':hashlib.sha256(candidate.read_bytes()).hexdigest(),'reference':hashlib.sha256(reference.read_bytes()).hexdigest()}}
        (reference.parent/'identity.json').write_text(json.dumps(ident))
        return candidate,reference

    def run_case(self,case='fate-matroska-xiph-lacing',demux=True):
        argv=['run_fate.py','--case',case]+(['--demux-only'] if demux else [])
        with patch.object(sys,'argv',argv),contextlib.redirect_stdout(io.StringIO()),contextlib.redirect_stderr(io.StringIO()):
            return fate.main()

    def test_demux_only_rejects_no_workers_without_running_upstream(self):
        with patch.object(fate.subprocess,'run') as command,patch.object(fate,'parity') as compare:
            with self.assertRaisesRegex(ValueError,'candidate and reference workers'):
                self.run_case()
            command.assert_not_called()
            compare.assert_not_called()

    def test_demux_only_rejects_one_missing_worker(self):
        candidate,reference=self.adapters()
        reference.unlink()
        with self.assertRaisesRegex(ValueError,'candidate and reference workers'):
            self.run_case()
        self.assertTrue(candidate.exists())

    def test_original_upstream_only_mode_remains_explicit(self):
        with patch.object(fate.subprocess,'run',return_value=fate.subprocess.CompletedProcess([],0,'native FATE output','')) as command,patch.object(fate,'parity') as compare:
            self.assertEqual(self.run_case(demux=False),0)
            command.assert_called_once()
            compare.assert_not_called()
        receipt=json.loads((self.root/'.research/reference/fate-xiph.json').read_text())
        self.assertTrue(receipt['upstream_case_executed'])
        self.assertEqual(receipt['candidate_status'],'not tested by upstream FATE')

    def test_opus_vector_uses_original_demux_comparator_and_keeps_remux_unexecuted(self):
        candidate,reference=self.adapters()
        with patch.object(fate,'parity',return_value={'status':'pass'}) as compare,patch.object(fate.subprocess,'run') as command:
            self.assertEqual(self.run_case('fate-matroska-opus-remux'),0)
            compare.assert_called_once_with(candidate,reference,self.root/'.research/fate-samples/mkv/codec_delay_opus.mkv',self.root/'.research/fate-candidate-opus',sequential_required=True)
            command.assert_not_called()
        receipt=json.loads((self.root/'.research/reference/fate-opus.json').read_text())
        self.assertFalse(receipt['upstream_case_executed'])
        self.assertEqual(receipt['reference_status'],'not-run-demux-only')
        self.assertEqual(receipt['candidate_demux_comparison']['status'],'pass')
        self.assertIn('not complete upstream FATE execution',receipt['candidate_status'])
        self.assertIn('redistribution unreviewed',receipt['rights'])

    def test_opus_parity_failure_remains_a_failure(self):
        self.adapters()
        with patch.object(fate,'parity',return_value={'status':'mismatch'}):
            self.assertEqual(self.run_case('fate-matroska-opus-remux'),1)

    def test_changed_input_refuses_the_old_receipt(self):
        self.adapters()
        receipt=self.root/'.research/reference/fate-xiph.json'
        receipt.write_text(json.dumps({'sample_sha256':'changed-input'}))
        with patch.object(fate,'parity') as compare:
            with self.assertRaisesRegex(ValueError,'FATE input changed'):
                self.run_case()
            compare.assert_not_called()

    def test_stale_adapters_are_rejected_before_comparison(self):
        self.adapters()
        with patch.object(fate,'source_identity_error',return_value='changed-source'),patch.object(fate,'parity') as compare:
            with self.assertRaisesRegex(ValueError,'Stale adapter identity'):
                self.run_case()
            compare.assert_not_called()

    def test_upstream_failure_cannot_be_overridden_by_candidate_pass(self):
        self.adapters()
        with patch.object(fate.subprocess,'run',return_value=fate.subprocess.CompletedProcess([],7,'','native failure')),patch.object(fate,'parity',return_value={'status':'pass'}):
            self.assertEqual(self.run_case(demux=False),7)
