"""Independent Python/zlib content semantics, including reference disagreements."""
import json,subprocess,tempfile,unittest,zlib
from pathlib import Path
from .runner import ROOT
from .spec_oracle import elements,variable
from .common import zlib_fixture
from .content_oracle import content_trace, projected_content

class ContentTests(unittest.TestCase):
    def test_ordered_chain_empty_and_private_bytes_against_independent_oracle(self):
        for options in [dict(empty=True),dict(mixed=True),dict(scope=2,aac=True),dict(scope=3)]:
            data=zlib_fixture(**options)
            _,start,stop,_=next(x for x in elements(data,0,len(data)) if x[0]==0x18538067)
            top=list(elements(data,start,stop));encodings=[];private=b'';frame=None
            _,a,b,_=next(x for x in top if x[0]==0x1654ae6b)
            _,a,b,_=next(x for x in elements(data,a,b) if x[0]==0xae)
            for key,x,y,_ in elements(data,a,b):
                if key==0x63a2:private=data[x:y]
                if key!=0x6d80:continue
                for _,c,d,_ in elements(data,x,y):
                    order,scope,algorithm,header=0,1,0,b''
                    for key,c,d,_ in elements(data,c,d):
                        if key==0x5031:order=int.from_bytes(data[c:d],'big')
                        if key==0x5032:scope=int.from_bytes(data[c:d],'big')
                        if key==0x5034:
                            for key,c,d,_ in elements(data,c,d):
                                if key==0x4254:algorithm=int.from_bytes(data[c:d],'big')
                                if key==0x4255:header=data[c:d]
                    encodings.append((order,scope,algorithm,header))
            _,a,b,_=next(x for x in top if x[0]==0x1f43b675)
            _,a,b,_=next(x for x in elements(data,a,b) if x[0]==0xa3)
            _,header,_=variable(data,a);frame=data[header+3:b]
            for _,scope,algorithm,prefix in sorted(encodings,reverse=True):
                decode=lambda encoded: zlib.decompress(encoded) if algorithm==0 else prefix+encoded
                if scope&1:frame=decode(frame)
                if scope&2:private=decode(private)
            with tempfile.TemporaryDirectory() as directory:
                path=Path(directory)/'encoded.mkv';path.write_bytes(data)
                output=json.loads(subprocess.check_output([ROOT/'target/release/media-worker','trace',path],text=True))
            self.assertEqual(bytes.fromhex(output['tracks'][0]['private']),private)
            self.assertEqual([bytes.fromhex(p['payload']) for p in output['packets']],[frame])

    def test_optional_pcm_decoder_view_preserves_the_full_source_event(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'empty.mkv';path.write_bytes(zlib_fixture(empty=True))
            worker=ROOT/'target/release/media-worker'
            source=json.loads(subprocess.check_output([worker,'trace',path],text=True))
            timing=json.loads(subprocess.check_output([worker,'trace-source',path],text=True))
            projected=json.loads(subprocess.check_output([worker,'codec-trace',path],text=True))
            native=json.loads(subprocess.check_output([ROOT/'target/harness/ffmpeg-worker','codec-trace',path],text=True))
            self.assertEqual(len(source['packets']),1)
            self.assertEqual(source['packets'][0]['payload'],'')
            self.assertEqual(len(timing['packets']),1)
            self.assertEqual(projected,native)
            self.assertEqual(projected['packets'],[])
            # This is a narrow PCM view, never a universal empty-frame filter.
            path.write_bytes(zlib_fixture(empty=True).replace(b'A_PCM/INT/LIT',b'A_PCM/INT/BIG'))
            opaque=json.loads(subprocess.check_output([worker,'codec-trace',path],text=True))
            self.assertEqual(len(opaque['packets']),1)


class SourceOracleTests(unittest.TestCase):
    def test_all_selected_compression_bytes_match_an_independent_source_oracle(self):
        worker = ROOT / 'target/release/media-worker'
        for case in json.loads((ROOT / 'harness/zlib-corpus.json').read_text())['cases']:
            path = ROOT / '.research/zlib-fixtures' / case['file']
            trace = json.loads(subprocess.check_output([worker, 'trace', path]))
            self.assertEqual(projected_content(trace), content_trace(path), case['id'])

    def test_same_difference_paths_do_not_allow_changed_native_values(self):
        import hashlib
        from .runner import review_native_difference
        source = {'packets': [{'payload': '02'}]}
        native = {'packets': [{'payload': '01'}]}
        rule = {'id': 'test', 'raw_native_difference_paths': ['/packets/0/payload'],
                'raw_native_output_sha256': hashlib.sha256(json.dumps(native, sort_keys=True, separators=(',', ':')).encode()).hexdigest()}
        self.assertEqual(review_native_difference(source, native, rule)[1]['status'], 'reviewed')
        altered = {'packets': [{'payload': '03'}]}
        self.assertEqual(review_native_difference(source, altered, rule)[1]['status'], 'unexpected-native-difference')
