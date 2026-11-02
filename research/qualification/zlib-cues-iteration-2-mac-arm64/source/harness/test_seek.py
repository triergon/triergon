"""Reference API initialization and source preroll regression checks."""
import json
import subprocess
import unittest
from .runner import ROOT


class SeekTests(unittest.TestCase):
    def test_reference_unknown_cluster_fresh_and_after_eof_agree(self):
        worker = ROOT / 'target/harness/ffmpeg-worker'
        path = ROOT / '.research/seek-fixtures/cues-unknown-cluster.mkv'
        for target in [0, 250_000_000, 2_500_000_000, 7_500_000_000, 15_500_000_000, 25_000_000_000]:
            args = [worker, 'seek-trace', path, str(target)]
            fresh = json.loads(subprocess.check_output(args, text=True))
            replay = json.loads(subprocess.check_output([*args, 'after-eof'], text=True))
            self.assertEqual(fresh, replay, f'seek target {target}')
