"""Whole-input catalogue/source parity and complete measured-work checks."""
import json,unittest,tempfile
from pathlib import Path
from .runner import ROOT,execute,differences,catalogue_parity
from .catalogue_oracle import source_catalogue
from .test_checksum import scalar_trace_checksum

class CatalogueTests(unittest.TestCase):
    def cases(self):
        return [ROOT/'.research/catalogue-fixtures'/c['file'] for c in json.loads((ROOT/'harness/features/matroska-segment-metadata/corpus.json').read_text())['cases']]

    def test_native_catalogue_exact_source_and_all_checksum_work(self):
        for path in self.cases():
            with self.subTest(case=path.stem):
                left=execute([ROOT/'target/release/media-worker','catalogue-trace',path]);right=execute([ROOT/'target/harness/ffmpeg-worker','catalogue-trace',path])
                self.assertEqual(left['status'],'ok',left);self.assertEqual(right['status'],'ok',right)
                self.assertEqual(differences(left['data'],right['data']),[])
                source=execute([ROOT/'target/release/media-worker','catalogue-source',path]);self.assertEqual(source['status'],'ok',source)
                self.assertEqual(differences(source['data'],source_catalogue(path)),[])
                expected=scalar_trace_checksum(left['data'])
                for worker in [ROOT/'target/release/media-worker',ROOT/'target/harness/ffmpeg-worker']:
                    work=execute([worker,'catalogue-bench',path,1]);self.assertEqual(work['status'],'ok',work)
                    self.assertEqual(work['data']['digest'],expected)
                    self.assertEqual(work['data']['packets'],len(left['data']['packets']))
                    self.assertEqual(work['data']['bytes'],sum(len(p['payload'])//2 for p in left['data']['packets']))

    def test_original_hdr_vector_is_not_stripped_and_matches_complete_protocol(self):
        path=ROOT/'.research/fate-samples/mkv/hdr10tags-both.mkv'
        self.assertTrue(path.is_file(),'Fetch original required HDR FATE vector before this gate')
        with tempfile.TemporaryDirectory() as directory:
            result=catalogue_parity(ROOT/'target/release/media-worker',ROOT/'target/harness/ffmpeg-worker',path,Path(directory))
            self.assertEqual(result['status'],'pass',result)
            self.assertEqual(result['source_catalogue_status'],'pass');self.assertEqual(result['source_colour_status'],'pass')

