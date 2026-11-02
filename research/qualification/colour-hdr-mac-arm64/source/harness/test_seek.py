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

    def test_codec_delay_audio_is_retained_without_repeating_initial_trim(self):
        workers = [ROOT / 'target/release/media-worker', ROOT / 'target/harness/ffmpeg-worker']
        path = ROOT / '.research/seek-fixtures/cues-delay-preroll.mkv'
        for worker in workers:
            for target, first_ns, trim in [(0, -50_000_000, 400), (250_000_000, -50_000_000, 400), (2_500_000_000, 1_950_000_000, 0), (7_500_000_000, 6_950_000_000, 0), (15_500_000_000, 14_950_000_000, 0), (25_000_000_000, 14_950_000_000, 0)]:
                for suffix in [[], ['after-eof']]:
                    with self.subTest(worker=worker.name, target=target, state=suffix):
                        trace = json.loads(subprocess.check_output([worker, 'seek-trace', path, str(target), *suffix], stderr=subprocess.PIPE))
                        first = trace['packets'][0]
                        self.assertEqual(first['track'], 1)
                        self.assertEqual(first['pts_ns'], first_ns)
                        self.assertEqual(first['trim_start_samples'], trim)
                        self.assertEqual(first['payload'], bytes(range(32)).hex())
