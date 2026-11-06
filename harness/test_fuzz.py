import copy
import json
from pathlib import Path
import random
import subprocess
import sys
import tempfile
import unittest

from .common import digest, digest_bytes
from .fuzz import compare, mutation, observe, replay, strict_json
from .runner import ROOT


class MutationTests(unittest.TestCase):
    def test_replay_binds_seed_result_and_edit_order(self):
        seed=b'\0\xffabc'
        expected=b'\0Zbc!'
        recipe={'schema':1,'seed_sha256':digest_bytes(seed),'result_sha256':digest_bytes(expected),
                'edits':[{'offset':1,'remove':2,'insert_hex':'5a'},
                         {'offset':4,'remove':0,'insert_hex':'21'}]}
        self.assertEqual(replay(seed,recipe,64),expected)
        with self.assertRaises(ValueError):replay(seed+b'x',recipe,64)
        changed=copy.deepcopy(recipe);changed['edits'].reverse()
        with self.assertRaises(ValueError):replay(seed,changed,64)
        changed=copy.deepcopy(recipe);changed['result_sha256']='0'*64
        with self.assertRaises(ValueError):replay(seed,changed,64)

    def test_invalid_or_unbounded_edits_never_use_python_negative_slicing(self):
        seed=b'abcd'
        base={'schema':1,'seed_sha256':digest_bytes(seed),'result_sha256':digest_bytes(seed),
              'edits':[{'offset':0,'remove':0,'insert_hex':''}]}
        for edit in [{'offset':-1,'remove':0,'insert_hex':''},
                     {'offset':True,'remove':0,'insert_hex':''},
                     {'offset':0,'remove':5,'insert_hex':''},
                     {'offset':5,'remove':0,'insert_hex':''},
                     {'offset':0,'remove':0,'insert_hex':'00'*5},
                     {'offset':0,'remove':0,'insert_hex':' 0'},
                     {'offset':0,'remove':0,'insert_hex':'g0'}]:
            case=copy.deepcopy(base);case['edits']=[edit]
            with self.assertRaises(ValueError):replay(seed,case,8)
        base['schema']=True
        with self.assertRaises(ValueError):replay(seed,base,8)

    def test_empty_and_full_inputs_mutate_reproducibly_within_absolute_bound(self):
        for seed in [b'',b'\0',bytes(range(256)),b'x'*4096]:
            first,second=random.Random(113),random.Random(113)
            for _ in range(60):
                data,recipe=mutation(seed,first,max(1,len(seed)))
                again,other=mutation(seed,second,max(1,len(seed)))
                self.assertEqual((data,recipe),(again,other))
                self.assertNotEqual(data,seed)
                self.assertLessEqual(len(data),max(1,len(seed)))
                self.assertEqual(replay(seed,recipe,max(1,len(seed))),data)

    def test_strict_protocol_does_not_hide_duplicate_fields_or_non_finite_numbers(self):
        for raw in [b'{"payload":"01","payload":"02"}',b'{"x":NaN}',b'{"x":Infinity}']:
            with self.assertRaises(ValueError):strict_json(raw)

    def test_output_comparison_preserves_order_unknown_values_and_boolean_types(self):
        def ok(data):return {'status':'ok','data':data}
        left={'packets':[{'payload':'01','dts':None,'key':False},{'payload':'02'}]}
        right={'packets':[{'payload':'02'},{'payload':'01','dts':0,'key':0}]}
        result=compare({'candidate':ok(left),'reference':ok(right)})
        self.assertEqual(result['findings'][0]['kind'],'output-disagreement')
        self.assertFalse(result['qualification'])
        self.assertTrue(compare({'candidate':ok(False),'debug':ok(0)})['findings'])

    def test_rejections_and_missing_evidence_are_never_feature_acceptance(self):
        rejected={'status':'rejected','returncode':1,'stderr_sha256':'e'}
        result=compare({'candidate':rejected,'reference':rejected})
        self.assertEqual(result['comparisons'][0]['status'],'both-rejected-ungraded')
        self.assertFalse(result['qualification'])
        with self.assertRaises(ValueError):compare({})
        with self.assertRaises(ValueError):compare({'candidate':rejected})
        result=compare({'candidate':rejected,'reference':{'status':'ok','data':{}}})
        self.assertEqual(result['findings'][0]['kind'],'acceptance-disagreement')

    def test_worker_timeout_is_retained_and_only_its_own_process_is_terminated(self):
        result,_,_=observe([sys.executable,'-c','import time;time.sleep(3)'],.1,4096)
        self.assertEqual(result['status'],'timeout')
        self.assertLess(result['returncode'],0)

    def test_stdout_and_stderr_are_bounded_together(self):
        result,stdout,stderr=observe([sys.executable,'-c',
            'import os;os.write(1,b"x"*4096);os.write(2,b"y"*4096)'],3,1024)
        self.assertEqual(result['status'],'output-limit')
        self.assertTrue(result['output_truncated'])
        self.assertLessEqual(len(stdout)+len(stderr),1024)

    def test_success_error_crash_and_invalid_protocol_remain_distinct(self):
        cases=[('print("{\\"value\\":1}")','ok'),
               ('import sys;sys.stderr.write("rejected");sys.exit(1)','rejected'),
               ('import sys;sys.exit(101)','crash'),('print("not json")','invalid-json')]
        for source,expected in cases:
            result,_,_=observe([sys.executable,'-c',source],3,4096)
            self.assertEqual(result['status'],expected)

    def test_campaign_keeps_replay_inputs_raw_disagreements_and_never_qualifies(self):
        (ROOT/'.research').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='fuzz-cli-test-',dir=ROOT/'.research') as temporary:
            directory=Path(temporary)
            left,right=directory/'left.py',directory/'right.py'
            left.write_text('import json,pathlib,sys;print(json.dumps({"bytes":len(pathlib.Path(sys.argv[1]).read_bytes())}))\n')
            right.write_text('import json,pathlib,sys;print(json.dumps({"bytes":len(pathlib.Path(sys.argv[1]).read_bytes())+1}))\n')
            seed=directory/'seed.bin';seed.write_bytes(b'\0\xfftest')
            roles={role:{'path':sys.executable,'sha256':digest(sys.executable)} for role in ('candidate','debug','reference')}
            operation={'id':'trace','roles':{'candidate':[str(left),'{input}'],'debug':[str(left),'{input}'],
                                            'reference':[str(right),'{input}']}}
            manifest={'schema':1,'workers':roles,'source_files':{str(left):digest(left),str(right):digest(right)},
                      'operations':[operation],'seeds':[{'id':'binary','path':str(seed),'sha256':digest(seed),
                        'rights':'authored-synthetic','baseline_statuses':{'trace':dict.fromkeys(roles,'ok')}}]}
            source=directory/'manifest.json';source.write_text(json.dumps(manifest))
            output=directory/'output'
            done=subprocess.run([sys.executable,str(ROOT/'tools/fuzz_media.py'),'--manifest',str(source),
                                 '--output',str(output),'--rounds','1'],capture_output=True,timeout=15)
            self.assertEqual(done.returncode,1,done.stderr.decode())
            report=json.loads((output/'report.json').read_text())
            self.assertEqual(report['collection_status'],'complete')
            self.assertEqual(report['cases'],2)
            self.assertEqual(report['findings_by_kind'],{'output-disagreement':2})
            self.assertFalse(report['qualification'])
            self.assertTrue((output/'case-000000/trace/reference.stdout').exists())
            case=json.loads((output/'case-000001/observation.json').read_text())
            self.assertEqual(replay(seed.read_bytes(),case['recipe'],4*1024*1024),
                             (output/'case-000001/input.bin').read_bytes())
            rows=(output/'findings.jsonl').read_text().splitlines()
            self.assertEqual(len(rows),2)
            self.assertTrue(report['source_workers_and_seeds_unchanged'])
            identity=json.loads((output/'identity.json').read_text())
            for relative,expected in identity['controller_sha256'].items():
                self.assertEqual(digest(output/'controller-source'/relative),expected)

    def test_campaign_rejects_output_path_labels_before_starting_workers(self):
        (ROOT/'.research').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='fuzz-label-test-',dir=ROOT/'.research') as temporary:
            directory=Path(temporary)
            manifest={'schema':1,'workers':{'candidate':{},'../../escape':{}},
                      'operations':[{'id':'trace'}],'seeds':[{'id':'seed'}]}
            path=directory/'manifest.json';path.write_text(json.dumps(manifest))
            done=subprocess.run([sys.executable,str(ROOT/'tools/fuzz_media.py'),'--manifest',str(path),
                '--output',str(directory/'output')],capture_output=True,timeout=5)
            self.assertEqual(done.returncode,2)
            self.assertIn(b'safe role labels',done.stderr)
            self.assertFalse((directory/'output').exists())


if __name__=='__main__':unittest.main()
