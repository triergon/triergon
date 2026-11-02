import json
import tempfile
import unittest
from pathlib import Path
from .runner import ROOT, execute, differences
from .colour_oracle import source_colour


class ColourParityTests(unittest.TestCase):
    def test_every_frozen_colour_profile_matches_native_and_exact_source(self):
        manifest=json.loads((ROOT/'harness/colour-corpus.json').read_text())
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
        import importlib.util
        import random
        spec=importlib.util.spec_from_file_location('colour_generator',ROOT/'tools/generate-colour-corpus.py')
        fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
        rng=random.Random(9559)
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'mastering.mkv'
            for n in range(128):
                values=[rng.random() for _ in range(8)]+[rng.uniform(1,10000),rng.random()]
                path.write_bytes(fixture.make({},fixture.mastering(width=4 if n%2 else 8,values=values)))
                left=execute([ROOT/'target/release/media-worker','metadata-trace',path])
                right=execute([ROOT/'target/harness/ffmpeg-worker','metadata-trace',path])
                self.assertEqual(left['status'],right['status'],'seed9559 case '+str(n))
                self.assertEqual(differences(left.get('data'),right.get('data')),[],('seed9559',n,values))

    def test_metadata_benchmarks_consume_every_canonical_field(self):
        from .test_checksum import scalar_trace_checksum
        for row in json.loads((ROOT/'harness/colour-corpus.json').read_text())['cases']:
            path=ROOT/'.research/colour-fixtures'/row['file']
            for worker in [ROOT/'target/release/media-worker',ROOT/'target/harness/ffmpeg-worker']:
                trace=execute([worker,'metadata-trace',path])
                measured=execute([worker,'metadata-bench',path,1])
                self.assertEqual(trace['status'],measured['status'])
                self.assertEqual(measured['data']['digest'],scalar_trace_checksum(trace['data']))
                self.assertEqual(measured['data']['packets'],len(trace['data']['packets']))

    def test_missing_source_colour_proof_cannot_satisfy_the_gate(self):
        from .runner import acceptance
        contract=json.loads((ROOT/'harness/colour-slice.json').read_text())
        row={'id':'colour-absent','profile':'colour-absent','parity':{'status':'pass','source_timing_status':'pass'}}
        result=acceptance(contract,[row],{},'aarch64-apple-darwin',[])
        self.assertTrue(any('complete source/native colour gate' in r for r in result['slice_reasons']))
