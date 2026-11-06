"""A reviewed reference choice cannot become a global normalization or hash bypass."""
import copy
import json
import subprocess
import unittest
from .common import load
from .runner import ROOT
from .comparison import rule_for, reference_record, reference_summary, validate_case_evidence
from .content_oracle import content_trace, projected_content


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.registry = load(ROOT / 'harness/comparison-rules.json')
        self.identity = {'source_revision': self.registry['reference_source_revision'],
                         'comparison_rules': self.registry,
                         'binaries': {'candidate': 'rust', 'reference': 'native'},
                         'comparison_references': {
                             'ffmpeg-native': {'path': 'target/harness/ffmpeg-worker', 'sha256': 'native', 'kind': 'pinned native FFmpeg'},
                             'ffmpeg-combined-native': {'path': 'target/harness/ffmpeg-combined-worker', 'sha256': 'combined', 'kind': 'explicit combined reference'}}}
        self.case = next(c for c in load(ROOT / 'harness/zlib-corpus.json')['cases'] if c['id'] == 'zlib-mixed')

    def test_choice_is_bound_to_approval_source_input_profile_and_contract(self):
        rule = rule_for(self.case, self.identity, 'K2-bounded-zlib-v1')
        self.assertEqual(reference_record(self.identity, rule)['sha256'], 'combined')
        for field, value in [('sha256', 'changed'), ('profile', 'another'), ('seek_target_ns', 1)]:
            case = {**self.case, field: value}
            with self.assertRaises(ValueError):
                rule_for(case, self.identity, 'K2-bounded-zlib-v1')
        for contract in [None, 'K3-cues-preroll-v1']:
            with self.assertRaises(ValueError):
                rule_for(self.case, self.identity, contract)
        self.registry['approval']['status'] = 'pending'
        with self.assertRaises(ValueError):
            rule_for(self.case, self.identity, 'K2-bounded-zlib-v1')
        self.registry['approval']['status'] = 'approved'
        self.identity['source_revision'] = 'changed'
        with self.assertRaises(ValueError):
            rule_for(self.case, self.identity, 'K2-bounded-zlib-v1')

    def test_selected_worker_and_complete_source_proof_cannot_be_substituted(self):
        rule = rule_for(self.case, self.identity, 'K2-bounded-zlib-v1')
        record = reference_summary(reference_record(self.identity, rule))
        row = {'id': self.case['id'], 'profile': self.case['profile'], 'input_sha256': self.case['sha256'],
               'reference': record, 'operation': {'view': rule['view']},
               'parity': {'comparison_rule': rule['id'], 'source_content_status': 'pass',
                          'raw_native_disposition': {'status': 'reviewed', 'rule': rule['id']},
                          'raw_native_output_sha256': rule['raw_native_output_sha256']}}
        self.assertEqual(validate_case_evidence(row, self.identity, 'K2-bounded-zlib-v1'),
                         {'candidate': 'rust', 'reference': 'combined'})
        for field in ['source_content_status', 'raw_native_disposition', 'comparison_rule', 'raw_native_output_sha256']:
            changed = copy.deepcopy(row); changed['parity'].pop(field)
            with self.assertRaises(ValueError):
                validate_case_evidence(changed, self.identity, 'K2-bounded-zlib-v1')
        row['reference']['sha256'] = 'native'
        with self.assertRaises(ValueError):
            validate_case_evidence(row, self.identity, 'K2-bounded-zlib-v1')

    def test_all_selected_compression_bytes_match_an_independent_source_oracle(self):
        worker = ROOT / 'target/release/media-worker'
        for case in load(ROOT / 'harness/zlib-corpus.json')['cases']:
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
