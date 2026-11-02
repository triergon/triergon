"""Original kernel-bound controls; no compiler, module or native process runs."""
import copy
import hashlib
import json
from pathlib import Path
import struct
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from harness import memory
from harness.test_linux_port import receipt


def kernel_receipt():
    data = receipt("rss")
    data.update(observer="linux-glibc-v3", guard_policy=memory.GUARD_POLICY,
        guard_scope="observed-interposable-entrypoints", guard_order=list(memory.GUARD_ORDER),
        guard_resolved_mask=127, private_tls_basis="no-active-thread-or-loader-entrypoints",
        private_tls_direct_hooks=False, unknown_window_releases=0,
        guard_calls=[0]*7, guard_window_calls=[0]*7,
        guard_window_successes=[0]*7, guard_window_failures=[0]*7,
        kernel_rss_requested=1, kernel_rss_abi=1, kernel_rss_initial_bytes=4096*32,
        kernel_rss_positive_pages=16, kernel_rss_upper_bytes=4096*48,
        kernel_rss_events=7, kernel_rss_missed=0, kernel_rss_errors=0, kernel_rss_page_size=4096,
        rss_start=4096*32, rss_peak_observed=4096*40, rss_end=4096*32,
        virtual_peak_start=4096*64, virtual_peak_end=4096*64,
        rss_peak_upper_bound_bytes=4096*48,
        rss_peak_upper_bound_basis="min-kernel-positive-deltas-lifetime-VmPeak")
    return data


class KernelReceiptTests(unittest.TestCase):
    def test_declared_kernel_minimum_is_parseable_without_assigning_qualification(self):
        self.assertTrue(memory.valid_linux_receipt(kernel_receipt(), "rss"))

    def test_both_ceiling_branches_are_exact_and_no_virtual_denominator_is_introduced(self):
        data = kernel_receipt(); data["virtual_peak_end"] = 4096*44
        data["virtual_peak_start"] = data["virtual_peak_end"]
        data["rss_peak_upper_bound_bytes"] = 4096*44
        self.assertTrue(memory.valid_linux_receipt(data, "rss"))
        data["rss_peak_upper_bound_bytes"] -= 4096
        self.assertFalse(memory.valid_linux_receipt(data, "rss"))

    def test_kernel_arithmetic_errors_misses_ABI_and_boolean_counters_are_nonpasses(self):
        for field, value in (("kernel_rss_upper_bytes", 1), ("kernel_rss_missed", 1),
                             ("kernel_rss_errors", 1), ("kernel_rss_abi", 2),
                             ("kernel_rss_requested", True), ("kernel_rss_positive_pages", False),
                             ("kernel_rss_page_size", 8192), ("kernel_rss_events", 0)):
            data = kernel_receipt(); data[field] = value
            self.assertFalse(memory.valid_linux_receipt(data, "rss"), (field, value))

    def test_kernel_ceiling_below_observed_RSS_and_after_constructor_threads_are_nonpasses(self):
        data = kernel_receipt(); data["rss_peak_observed"] = data["kernel_rss_upper_bytes"]+4096
        self.assertFalse(memory.valid_linux_receipt(data, "rss"))
        data = kernel_receipt(); data["guard_calls"][3] = 1
        self.assertFalse(memory.valid_linux_receipt(data, "rss"))

    def test_kernel_basis_is_RSS_only_and_requires_requested_exactly_one(self):
        data = kernel_receipt(); data["mode"] = "heap"
        self.assertFalse(memory.valid_linux_receipt(data, "heap"))
        data = kernel_receipt(); data["kernel_rss_requested"] = 0
        self.assertFalse(memory.valid_linux_receipt(data, "rss"))

    def test_original_vmpeak_receipt_remains_a_distinct_readable_observation(self):
        self.assertTrue(memory.valid_linux_receipt(receipt("rss"), "rss"))


def module_elf():
    note = struct.pack("<III", 4, 20, 3)+b"GNU\0"+b"authored-build-id123"
    assert len(note) == 36
    names = b"\0.shstrtab\0.note.gnu.build-id\0.bss\0"
    sections_at = 64; names_at = sections_at+4*64; note_at = names_at+len(names)
    binary = bytearray(note_at+len(note)); binary[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<HH", binary, 16, 1, 183)
    struct.pack_into("<Q", binary, 40, sections_at)
    struct.pack_into("<HHH", binary, 58, 64, 4, 1)
    for index, name, kind, at, size in ((1,1,3,names_at,len(names)),(2,11,7,note_at,len(note)),(3,30,8,0x100000,0x400000)):
        where=sections_at+index*64
        struct.pack_into("<II",binary,where,name,kind);struct.pack_into("<QQ",binary,where+24,at,size)
    binary[names_at:names_at+len(names)] = names; binary[note_at:] = note
    return bytes(binary), note


class KernelProofTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.runtime = {path: (b"authored runtime identity " + path.encode()) for path in memory.KERNEL_RUNTIME_FILES}
        binary, note = module_elf(); self.runtime["/sys/module/safe_media_rss_bound/notes/.note.gnu.build-id"] = note
        files={}
        for role in memory.KERNEL_RSS_ROLES:
            path=self.root/role; path.write_bytes(binary if role=='module_binary' else b'authored header' if role.endswith('_api') else role.encode())
            files[role]=dict(path=role,sha256=memory.sha(path))
        review=self.root/'review.json'; review.write_text('{"review":"authored test proof"}\n')
        proof=dict(schema='linux-rss-kernel-bound-proof-v1',status='reviewed-conditional-upper-bound',abi=1,
            kernel_release=memory.platform.release(),architecture=memory.platform.machine(),page_size=4096,
            coverage=dict(complete_hooks=True,single_thread_since_constructor=True,full_thp_disabled_before_open=True,
                lifecycle='constructor-open-to-END-ioctl',review_artifact=dict(path='review.json',sha256=memory.sha(review))),
            files=files,runtime_files={path:hashlib.sha256(data).hexdigest() for path,data in self.runtime.items()})
        path=self.root/'proof.json';path.write_text(json.dumps(proof))
        self.profile=dict(proof=proof,proof_path='proof.json',proof_sha256=memory.sha(path))
        self.observer=files['observer_binary']['sha256'];self.probe=files['probe_binary']['sha256']
        original=Path.read_bytes
        def read(path):return self.runtime[str(path)] if str(path) in self.runtime else original(path)
        for context in (patch.object(memory,'ROOT',self.root),patch.object(memory.platform,'system',return_value='Linux'),
                        patch.object(memory.os,'sysconf',return_value=4096),patch.object(Path,'read_bytes',read)):
            context.start();self.addCleanup(context.stop)

    def records(self):
        result=[]
        for case in ('default','writev','transient'):
            count=32*1024*1024 if case=='transient' else 4*1024*1024
            data=kernel_receipt();data.update(kernel_rss_positive_pages=count//4096+16,
                kernel_rss_upper_bytes=4096*48+count,virtual_peak_start=4096*64+count,
                virtual_peak_end=4096*64+count,rss_peak_upper_bound_bytes=4096*48+count,
                rss_peak_observed=4096*40+count)
            result.append(dict(case=case,observed=dict(status='measured',instrument_sha256=self.observer,observer=data,
                work=dict(calibration=case,temporary_requested_bytes=count,expected_release_equal=True,expect_guard_invalid=False))))
        return result

    def test_module_elf_note_is_exact_and_NOBITS_does_not_read_nonexistent_file_storage(self):
        binary,note=module_elf();self.assertEqual(memory.elf_build_id_note(binary),note)
        for changed in (binary[:50],b'not ELF'+binary[7:]):
            with self.assertRaises(ValueError):memory.elf_build_id_note(changed)

    def test_complete_profile_and_three_distinct_kernel_controls_certify(self):
        self.assertTrue(memory.kernel_profile_current(self.profile,self.observer,self.probe))
        cert=memory.certify_kernel(self.records(),self.profile,self.observer,self.probe)
        self.assertTrue(memory.kernel_certificate_valid(cert,self.observer,self.probe))

    def test_changed_loaded_module_and_changed_proof_bytes_are_nonpasses(self):
        self.runtime['/sys/module/safe_media_rss_bound/notes/.note.gnu.build-id']=b'wrong loaded module'
        self.assertFalse(memory.kernel_profile_current(self.profile,self.observer,self.probe))

    def test_changed_module_source_binary_or_coverage_attestation_cannot_be_reused(self):
        for role in ('module_source','module_binary','observer_source','observer_binary','module_api'):
            path=self.root/role;original=path.read_bytes();path.write_bytes(original+b'changed')
            self.assertFalse(memory.kernel_profile_current(self.profile,self.observer,self.probe),role);path.write_bytes(original)
        changed=copy.deepcopy(self.profile);changed['proof']['coverage']['complete_hooks']=False
        self.assertFalse(memory.kernel_profile_current(changed,self.observer,self.probe))
        (self.root/'proof.json').write_text('{}')
        self.assertFalse(memory.kernel_profile_current(self.profile,self.observer,self.probe))

    def test_missing_transient_duplicate_control_and_failed_marker_do_not_certify(self):
        for mutation in ('missing','duplicate','marker','thread','missed','no-positive-growth'):
            records=self.records()
            if mutation=='missing':records.pop()
            elif mutation=='duplicate':records.append(copy.deepcopy(records[0]))
            elif mutation=='marker':records[0]['observed']['observer']['ends']=0
            elif mutation=='thread':records[0]['observed']['observer']['guard_calls'][3]=1
            elif mutation=='missed':records[0]['observed']['observer']['kernel_rss_missed']=1
            else:records[0]['observed']['observer']['kernel_rss_positive_pages']=1
            with self.assertRaises(ValueError):memory.certify_kernel(records,self.profile,self.observer,self.probe)

    def test_identity_selects_an_exact_parent_proof_and_stale_hash_is_rejected(self):
        path=self.root/'target/harness';path.mkdir(parents=True)
        identity=dict(instrumentation=dict(rss_kernel_bound={key:self.profile[key] for key in ('proof_path','proof_sha256')}))
        (path/'identity.json').write_text(json.dumps(identity))
        self.assertEqual(memory.load_kernel_profile(self.observer,self.probe),self.profile)
        identity['instrumentation']['rss_kernel_bound']['proof_sha256']='0'*64
        (path/'identity.json').write_text(json.dumps(identity))
        with self.assertRaises(ValueError):memory.load_kernel_profile(self.observer,self.probe)


class KernelOperationTests(unittest.TestCase):
    def test_guard_controls_ignore_inherited_kernel_opt_in_and_explicit_RSS_sets_it(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);out=root/'target/harness';out.mkdir(parents=True);(out/'allocations.so').write_bytes(b'authored observer')
            data=receipt('rss');seen=[]
            def run(argv,**kwargs):
                env=kwargs['env'];seen.append(env.get('SAFE_MEDIA_RSS_KERNEL_BOUND'))
                Path(env['SAFE_MEDIA_ALLOCATION_REPORT']).write_text(json.dumps(data))
                return SimpleNamespace(returncode=0,stdout=b'{"calibration":"default"}',stderr=b'')
            with patch.object(memory,'ROOT',root),patch.object(memory.platform,'system',return_value='Linux'), \
                    patch.dict('os.environ',{'SAFE_MEDIA_RSS_KERNEL_BOUND':'1'},clear=True),patch.object(memory.subprocess,'run',side_effect=run):
                self.assertEqual(memory.observe(out/'memory-check',None,None,'rss')['status'],'measured')
                data=kernel_receipt()
                self.assertEqual(memory.observe(out/'memory-check',None,None,'rss',kernel_bound=True)['status'],'measured')
                self.assertEqual(memory.observe(out/'memory-check',None,None,'heap',kernel_bound=True)['status'],'observer-environment-invalid')
            self.assertEqual(seen,[None,'1'])

    def test_kernel_sample_without_a_certificate_never_assigns_memory_qualification(self):
        row={name:dict(status='measured',observer=kernel_receipt(),instrument_sha256='observer',work={})
             for name in ('candidate_rss','reference_rss')}
        result=memory.evaluate([row],1,0,dict(platform='Linux',observer_sha256='observer'),lambda *_:self.fail('interval must not execute'))
        self.assertEqual(result['status'],'observer-unqualified')


class TimingModuleGuardTests(unittest.TestCase):
    def timing(self, state, execute=None):
        import contextlib
        from harness import runner
        def worker(argv):
            return dict(status='ok', data=dict(elapsed_ns=79 if argv[0]=='candidate' else 100,
                iterations=1,packets=1,bytes=1,digest=1))
        with patch.object(runner,'timing_module_state',side_effect=state if callable(state) else None,
                          return_value=state if not callable(state) else None,create=True), \
                patch.object(runner,'competing_builds',return_value=[]),patch.object(runner,'cpu_snapshot',return_value={}), \
                patch.object(runner,'digest',return_value='authored-byte-identity'), \
                patch.object(runner,'benchmark_lock',return_value=contextlib.nullcontext()), \
                patch.object(runner.os,'getloadavg',return_value=(0,0,0)), \
                patch.object(runner,'execute',side_effect=execute or worker) as launched:
            result=runner.benchmark_once('candidate','reference','authored-input',2,1,0)
        return result,launched

    def test_loaded_module_blocks_every_warmup_and_timed_launch(self):
        result,worker=self.timing(dict(status='module-loaded',path='/sys/module/safe_media_rss_bound'))
        self.assertEqual(result['status'],'timing-module-loaded');worker.assert_not_called()

    def test_unreadable_module_state_is_a_nonpass_before_workers_launch(self):
        result,worker=self.timing(dict(status='unavailable',error='authored read failure'))
        self.assertEqual(result['status'],'timing-environment-invalid');worker.assert_not_called()

    def test_loading_during_first_timed_launch_preserves_that_result_without_another_launch(self):
        calls=[]
        def state():return dict(status='module-loaded' if len(calls)>=3 else 'module-absent')
        def worker(argv):
            calls.append(argv)
            return dict(status='ok',data=dict(elapsed_ns=79,iterations=1,packets=1,bytes=1,digest=1))
        result,worker=self.timing(state,worker)
        self.assertEqual(result['status'],'timing-module-loaded');self.assertEqual(worker.call_count,3)
        self.assertEqual(len(result['samples']),1)
        self.assertTrue(any(value.get('status')=='ok' for value in result['samples'][0].values() if isinstance(value,dict)))
        self.assertNotIn('ratio',result)

    def test_absence_is_recorded_and_normal_timing_semantics_continue(self):
        result,worker=self.timing(dict(status='module-absent'))
        self.assertEqual(result['status'],'measured');self.assertEqual(worker.call_count,6)
        self.assertTrue(result['timing_module_observations'])
        self.assertTrue(all(row['state']['status']=='module-absent' for row in result['timing_module_observations']))

    def test_Darwin_does_not_read_Linux_module_state(self):
        from harness import runner
        with patch.object(runner.platform,'system',return_value='Darwin'),patch.object(Path,'stat',side_effect=AssertionError('Linux sysfs must not be read')):
            self.assertEqual(runner.timing_module_state()['status'],'not-applicable')

    def test_module_presence_absence_and_missing_sysfs_are_distinct(self):
        from harness import runner
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);module=root/'safe_media_rss_bound'
            with patch.object(runner,'LINUX_MODULE_ROOT',root,create=True),patch.object(runner.platform,'system',return_value='Linux'):
                self.assertEqual(runner.timing_module_state()['status'],'module-absent')
                module.mkdir();self.assertEqual(runner.timing_module_state()['status'],'module-loaded')
                module.rmdir();root.rmdir();self.assertEqual(runner.timing_module_state()['status'],'unavailable')


if __name__ == "__main__": unittest.main()
