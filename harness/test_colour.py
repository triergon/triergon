import json
import tempfile
import unittest
from pathlib import Path
from .runner import ROOT, execute, differences
from .colour_oracle import source_colour


class ColourParityTests(unittest.TestCase):
    def test_every_frozen_colour_profile_matches_native_and_exact_source(self):
        manifest=json.loads((ROOT/'harness/features/matroska-colour/corpus.json').read_text())
        for row in manifest['cases']:
            with self.subTest(profile=row['profile']):
                path=ROOT/'.research/colour-fixtures'/row['file']
                left=execute([ROOT/'target/release/media-worker','metadata-trace',path])
                right=execute([ROOT/'target/harness/ffmpeg-worker','metadata-trace',path])
                self.assertEqual(left['status'],'ok',left)
                self.assertEqual(right['status'],'ok',right)
                self.assertEqual(differences(left['data'],right['data']),[])
                source=execute([ROOT/'target/release/media-worker','metadata-source',path])
                self.assertEqual(source['status'],'ok',source)
                self.assertEqual(differences(source['data'],source_colour(path)),[])

    def test_seeded_mastering_rationals_match_the_public_native_representation(self):
        import random
        from .common import xtask_fixture
        rng=random.Random(9559)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'mastering.mkv'
            for n in range(128):
                values=[rng.random() for _ in range(8)]+[rng.uniform(1,10000),rng.random()]
                path.write_bytes(xtask_fixture('colour','--mastering-width',4 if n%2 else 8,'--values',','.join(map(repr,values))))
                left=execute([ROOT/'target/release/media-worker','metadata-trace',path])
                right=execute([ROOT/'target/harness/ffmpeg-worker','metadata-trace',path])
                self.assertEqual(left['status'],right['status'],'seed9559 case '+str(n))
                self.assertEqual(differences(left.get('data'),right.get('data')),[],('seed9559',n,values))

    def test_metadata_benchmarks_consume_every_canonical_field(self):
        from .test_checksum import scalar_trace_checksum
        for row in json.loads((ROOT/'harness/features/matroska-colour/corpus.json').read_text())['cases']:
            path=ROOT/'.research/colour-fixtures'/row['file']
            for worker in [ROOT/'target/release/media-worker',ROOT/'target/harness/ffmpeg-worker']:
                trace=execute([worker,'metadata-trace',path])
                measured=execute([worker,'metadata-bench',path,1])
                self.assertEqual(trace['status'],measured['status'])
                self.assertEqual(measured['data']['digest'],scalar_trace_checksum(trace['data']))
                self.assertEqual(measured['data']['packets'],len(trace['data']['packets']))
