"""Synthetic controls for the actual seven-hook v3 schema; no native run."""
import copy
import hashlib
import unittest
from harness import memory as guard


def fixture():
    libc, loader, observer_path, probe_path = ("/sealed/" + name for name in ("libc.so.6", "ld-linux-aarch64.so.1", "allocations.so", "memory-check"))
    hashes = {path: hashlib.sha256(path.encode()).hexdigest() for path in (libc, loader, observer_path, probe_path)}
    libraries = [dict(name="libc.so.6", path=libc, realpath=libc, sha256=hashes[libc]),
                 dict(name="ld-linux-aarch64.so.1", path=loader, realpath=loader, sha256=hashes[loader])]
    linked = {name: dict(status="recorded", interpreter=loader, interpreter_sha256=hashes[loader], libraries=libraries,
                        inspected_artifact=dict(path=path, sha256=hashes[path]))
              for name, path in (("observer", observer_path), ("probe", probe_path))}
    public = "\n".join(name + "@@GLIBC_2.17 T 100 20" for name in guard.GUARD_ORDER)
    exports = "\n".join(name + " T 100 20" for name in guard.GUARD_ORDER)
    private = "\n".join(name + "@@GLIBC_PRIVATE T 100 20" for name in guard.TLS_IMPORTS)
    undefined = "\n".join(name + "@GLIBC_2.17 U" for name in guard.GUARD_ORDER)
    bindings = dict(observer_path=observer_path, probe_path=probe_path, observer_exports=exports,
        probe_undefined=undefined, defined_objects={libc: public, loader: private, observer_path: exports, probe_path: ""})
    records = []
    for mode in ("heap", "rss"):
        for case in ("default", "scope-startup", *guard.GUARD_CASES):
            data = dict(schema=2, observer="linux-glibc-v3", mode=mode, status="measured", error_bits=0,
                begins=1, ends=1, selected_thread_scope=1, unknown_window_releases=0,
                heap_peak_bytes=8192, heap_live_start=128, heap_live_end=128,
                rss_start=4096, rss_peak_observed=12288,
                guard_order=list(guard.GUARD_ORDER), guard_policy=guard.GUARD_POLICY,
                guard_scope="observed-interposable-entrypoints", guard_resolved_mask=127,
                private_tls_basis="no-active-thread-or-loader-entrypoints", private_tls_direct_hooks=False,
                guard_calls=[0]*7, guard_window_calls=[0]*7, guard_window_successes=[0]*7, guard_window_failures=[0]*7)
            work = dict(calibration=case, expect_guard_invalid=case in guard.GUARD_CASES,
                        expected_peak_bytes=8192, expected_release_equal=True,
                        guard_oracle_scope="authored-top-level-request-minima",
                        required_guard_window_calls=[0]*7, required_guard_window_successes=[0]*7, required_guard_window_failures=[0]*7)
            status = "measured"
            if case == "scope-startup": data["guard_calls"] = [2]*7
            elif case in guard.GUARD_CASES:
                index, successful = guard.GUARD_CASES[case]
                status = "observer-invalid"; data["status"] = "invalid"; data["error_bits"] = guard.SCOPE_ERROR_BIT
                data["guard_calls"][index] = data["guard_window_calls"][index] = 1
                data["guard_window_successes" if successful else "guard_window_failures"][index] = 1
                work["required_guard_window_calls"][index] = 1
                work["required_guard_window_successes" if successful else "required_guard_window_failures"][index] = 1
            observed = dict(status=status, instrument_sha256=hashes[observer_path], work=work)
            observed["receipt" if status == "observer-invalid" else "observer"] = data
            records.append(dict(mode=mode, case=case, observed=observed))
    return records, linked, hashes[observer_path], hashes[probe_path], bindings


class NativeSchemaGuardTests(unittest.TestCase):
    def test_complete_actual_v3_schema_and_twenty_eight_controls_certify(self):
        args = fixture(); certificate = guard.certify(*args)
        self.assertEqual(len(certificate["native_calibration_records"]), 28)
        self.assertTrue(guard.certificate_valid(certificate, args[1], args[2], args[3]))

    def test_each_missing_control_and_duplicates_are_nonpasses(self):
        args = fixture()
        for index in range(28):
            changed = copy.deepcopy(args); changed[0].pop(index)
            with self.assertRaises(ValueError): guard.certify(*changed)
        args[0].append(copy.deepcopy(args[0][0]))
        with self.assertRaises(ValueError): guard.certify(*args)

    def test_marker_failure_cannot_impersonate_an_active_guard(self):
        args = fixture(); data = args[0][2]["observed"]["receipt"]
        data["error_bits"] = 64
        with self.assertRaises(ValueError): guard.certify(*args)

    def test_startup_forwarding_still_requires_the_allocation_release_and_RSS_oracle(self):
        for mode, field, value in (("heap", "heap_peak_bytes", 8193), ("heap", "heap_live_end", 129),
                                  ("rss", "rss_peak_observed", 4096)):
            args = fixture()
            row = next(row for row in args[0] if row["case"] == "scope-startup" and row["mode"] == mode)
            row["observed"]["observer"][field] = value
            with self.assertRaises(ValueError): guard.certify(*args)

    def test_failed_mapping_and_loader_calls_must_be_seen_not_just_successes(self):
        args = fixture()
        for record in args[0]:
            if record["case"] == "guard-mmap-failure":
                data = record["observed"]["receipt"]
                data["guard_window_successes"][0] = 1; data["guard_window_failures"][0] = 0
        with self.assertRaises(ValueError): guard.certify(*args)

    def test_private_TLS_only_for_exact_libc_and_GL_PRIVATE_version(self):
        args = fixture(); cert = guard.certify(*args)
        owner = args[1]["probe"]["libraries"][0]
        raw = "mmap64@GLIBC_2.17 U\n_dl_allocate_tls@GLIBC_PRIVATE U\nother_alloc U\n"
        parsed = dict(uncovered=["mmap64", "_dl_allocate_tls", "other_alloc"], raw_listing=raw,
                      raw_undefined_symbols=[line.split()[0] for line in raw.splitlines()])
        result = guard.conditional_coverage(parsed, owner, cert, args[1], args[2], args[3])
        self.assertEqual(result["remaining_uncovered"], ["other_alloc"])
        self.assertEqual(result["uncovered"], parsed["uncovered"])
        result = guard.conditional_coverage(parsed, {**owner, "realpath": "/worker/fake-libc"}, cert, args[1], args[2], args[3])
        self.assertIn("_dl_allocate_tls", result["remaining_uncovered"])

    def test_uncovered_stays_uncovered_without_proof_or_with_untested_version(self):
        args = fixture(); cert = guard.certify(*args)
        parsed = dict(uncovered=["mmap64"], raw_listing="mmap64@GLIBC_UNTESTED U\n", raw_undefined_symbols=["mmap64@GLIBC_UNTESTED"])
        for evidence in (None, cert):
            result = guard.conditional_coverage(parsed, {}, evidence, args[1], args[2], args[3])
            self.assertEqual(result["remaining_uncovered"], ["mmap64"])

    def test_a_private_TLS_name_with_an_additional_untested_ABI_stays_uncovered(self):
        args = fixture(); cert = guard.certify(*args); owner = args[1]["probe"]["libraries"][0]
        raw = "_dl_allocate_tls@GLIBC_PRIVATE U\n_dl_allocate_tls@UNTESTED U\n"
        parsed = dict(uncovered=["_dl_allocate_tls"], raw_listing=raw,
                      raw_undefined_symbols=[line.split()[0] for line in raw.splitlines()])
        result = guard.conditional_coverage(parsed, owner, cert, args[1], args[2], args[3])
        self.assertEqual(result["remaining_uncovered"], ["_dl_allocate_tls"])

    def test_changed_provider_private_export_or_probe_invalidates_scope(self):
        args = fixture(); cert = guard.certify(*args)
        self.assertFalse(guard.certificate_valid(cert, args[1], args[2], "a"*64))
        args[4]["defined_objects"]["/sealed/libc.so.6"] += "\n_dl_allocate_tls@@GLIBC_PRIVATE T 100 20"
        with self.assertRaises(ValueError): guard.certify(*args)

    def test_active_sample_is_nonpass_even_after_calibration_and_measured_label(self):
        args = fixture(); cert = guard.certify(*args); data = copy.deepcopy(args[0][0]["observed"]["observer"])
        self.assertTrue(guard.sample_valid(data, cert, args[1], args[2], args[3]))
        data["guard_calls"][6] = data["guard_window_calls"][6] = data["guard_window_successes"][6] = 1
        self.assertFalse(guard.sample_valid(data, cert, args[1], args[2], args[3]))

    def test_unknown_window_free_or_boolean_counter_is_invalid(self):
        for mutation in ("free", "boolean"):
            args = fixture(); data = args[0][0]["observed"]["observer"]
            if mutation == "free": data["unknown_window_releases"] = 1
            else: data["guard_window_calls"][0] = False
            with self.assertRaises(ValueError): guard.certify(*args)

    def test_fatal_only_or_wrong_probe_minima_are_not_binding_proof(self):
        for mutation in ("fatal", "minima"):
            args = fixture(); observed = args[0][2]["observed"]
            if mutation == "fatal": observed.clear(); observed.update(status="observer-failed", exit_code=126)
            else: observed["work"]["required_guard_window_calls"] = [0]*7
            with self.assertRaises(ValueError): guard.certify(*args)





"""Mocked full-qualification controls; no native process/library executes."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

memory = guard


class GuardIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(); self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        out = self.root / "target/harness"; out.mkdir(parents=True)
        self.paths = {name: out / value for name, value in {
            "candidate": "candidate", "reference": "reference", "observer": "allocations.so",
            "probe": "memory-check", "libc": "libc.so.6", "loader": "ld-linux-aarch64.so.1"}.items()}
        for name, path in self.paths.items(): path.write_bytes(("authored " + name).encode())

    def test_probe_control_argument_is_forwarded_when_operation_is_none(self):
        from harness.test_linux_port import receipt
        data = receipt()
        def run(argv, **kwargs):
            self.assertEqual(argv, [str(self.paths["probe"]), "scope-startup"])
            Path(kwargs["env"]["SAFE_MEDIA_ALLOCATION_REPORT"]).write_text(json.dumps(data))
            return SimpleNamespace(returncode=0, stdout=b'{"calibration":"scope-startup"}', stderr=b"")
        with patch.object(memory, "ROOT", self.root), patch.object(memory.platform, "system", return_value="Linux"), \
                patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "run", side_effect=run):
            self.assertEqual(memory.observe(self.paths["probe"], None, None, "heap", ("scope-startup",))["status"], "measured")

    def test_full_qualification_runs_both_modes_and_never_admits_missing_native_guard_proof(self):
        from harness.test_linux_port import receipt
        records, _, _, _, _ = fixture()
        source = {(row["mode"], row["case"]): row["observed"] for row in records}
        libs = [dict(name=self.paths[role].name, path=str(self.paths[role]), realpath=str(self.paths[role]), sha256=memory.sha(self.paths[role]))
                for role in ("libc", "loader")]
        linked = {role: dict(status="recorded", interpreter=str(self.paths["loader"]), interpreter_sha256=memory.sha(self.paths["loader"]),
            libraries=libs, inspected_artifact=dict(path=str(self.paths[role]), sha256=memory.sha(self.paths[role])))
            for role in ("candidate", "reference", "probe", "observer")}
        public = "\n".join(name + "@@GLIBC_2.17 T 100 20" for name in memory.GUARD_ORDER)
        private = "\n".join(name + "@@GLIBC_PRIVATE T 100 20" for name in memory.TLS_IMPORTS)
        exports = "\n".join(name + " T 100 20" for name in memory.ALLOCATORS | {"write", "writev"} | set(memory.GUARD_ORDER))
        calls, corrupt_control = [], []
        def observe(worker, operation, path, mode, arguments=()):
            case = arguments[0] if arguments else "default"; calls.append((mode, case))
            raw = copy.deepcopy(source[(mode, case)])
            data = raw.get("observer", raw.get("receipt"))
            base = receipt(mode); base.update(data)
            raw["observer" if raw["status"] == "measured" else "receipt"] = base
            raw["instrument_sha256"] = memory.sha(self.paths["observer"])
            raw["work"].update(expected_peak_bytes=base["heap_peak_bytes"])
            if corrupt_control and case == "guard-munmap-failure":
                raw["status"] = "observer-failed"
            return raw
        def links(worker, interpreter=None):
            return linked[next(role for role, path in self.paths.items() if path == Path(worker))]
        def nm(argv, **kwargs):
            path = Path(argv[-1])
            if "--defined-only" in argv:
                return public if path == self.paths["libc"] else private if path == self.paths["loader"] else exports if path == self.paths["observer"] else ""
            if path == self.paths["probe"]: return "\n".join(name + "@GLIBC_2.17 U" for name in memory.GUARD_ORDER)
            if path == self.paths["libc"]: return "\n".join(name + "@GLIBC_PRIVATE U" for name in memory.TLS_IMPORTS)
            if path == self.paths["loader"]: return ""
            return "malloc@GLIBC_2.17 U\nfree@GLIBC_2.17 U\nmmap64@GLIBC_2.17 U\nmunmap@GLIBC_2.17 U\n"
        memory._GUARD_CALIBRATION_CACHE.clear()
        with patch.object(memory, "ROOT", self.root), patch.object(memory.platform, "system", return_value="Linux"), patch.dict("os.environ", {}, clear=True), \
                patch.object(memory, "observe", side_effect=observe), patch.object(memory, "linux_linked_libraries", side_effect=links), \
                patch.object(memory.subprocess, "check_output", side_effect=nm):
            result = memory.qualification(self.paths["candidate"], self.paths["reference"])
            memory._GUARD_CALIBRATION_CACHE.clear()
            corrupt_control.append(True)
            rejected = memory.qualification(self.paths["candidate"], self.paths["reference"])
        self.assertEqual(result["status"], "qualified-selected-workers", result["errors"])
        self.assertEqual(len(calls), 56)
        self.assertEqual(len(result["runtime_guard_certificate"]["native_calibration_records"]), 28)
        self.assertTrue(all(not row["remaining_uncovered"] for row in result["allocator_import_details"].values()))
        self.assertEqual(rejected["status"], "unqualified")
        self.assertIsNone(rejected["runtime_guard_certificate"])
        self.assertEqual(len(rejected["attempted_guard_calibration"]), 28)
        self.assertIn("mmap64", rejected["allocator_import_details"]["candidate:" + str(self.paths["candidate"])]["uncovered"])
        with patch.object(memory, "linux_loader_environment", return_value={}):
            self.assertTrue(memory.linux_certificate_current(result))
            self.paths["probe"].write_bytes(b"changed native probe after qualification")
            self.assertFalse(memory.linux_certificate_current(result))

    def test_a_prior_qualified_Linux_certificate_without_runtime_guard_proof_is_not_reusable(self):
        instrumentation = dict(platform="Linux", observer_sha256="observer", probe_sha256="probe",
                               linked_library_hashes={"libc": "old"})
        measurement = dict(status="qualified", pairs=30,
            certificate=dict(status="qualified-selected-workers", **instrumentation))
        self.assertFalse(memory.reusable_memory(measurement, 30, instrumentation))


if __name__ == "__main__": unittest.main()
