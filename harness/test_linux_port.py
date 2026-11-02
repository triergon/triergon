"""Linux harness controls using authored files/mocks; no native process starts."""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from . import memory, runner


def receipt(mode="heap"):
    return dict(schema=2, status="measured", mode=mode, observer="linux-glibc-v2",
        rss_source="proc-self-smaps_rollup-Rss", hwm_source="getrusage-RUSAGE_SELF-ru_maxrss-KiB-times-1024",
        hwm_scope="process-lifetime-reported", rss_exact_peak_status="unresolved", hwm_upper_bound_status="unproven",
        virtual_peak_source="proc-self-status-VmPeak", virtual_peak_start=300000, virtual_peak_end=400000,
        rss_peak_upper_bound_bytes=400000, rss_peak_upper_bound_basis="process-lifetime-VmPeak",
        rss_bound_scope="same-process-ordinary-mappings",
        rss_bound_formula="candidate-ceiling-minus-start-plus-two-pages-v1",
        rss_samples=8, error_bits=0, begins=1, ends=1, unknown_releases=0,
        recursive_calls=0, selected_thread_scope=1, glibc="authored-test-version", entry_capacity=65536,
        symbol_order=["malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc"],
        symbol_attempts=[1]*6, symbol_successes=[1]*6, symbol_failures=[0]*6,
        heap_peak_bytes=123, heap_live_start=16, heap_live_end=16,
        calls=10, window_allocation_calls=6, rss_start=100000,
        rss_peak_observed=200000, rss_end=110000, hwm_start=120000,
        hwm_end=200000, page_size=4096)


def bounded_receipt():
    data = receipt("rss")
    data.update(rss_peak_upper_bound_bytes=data["virtual_peak_end"],
                rss_peak_upper_bound_basis="process-lifetime-VmPeak")
    return data


class LinuxMemoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "target/harness").mkdir(parents=True)
        (self.root / "target/harness/allocations.so").write_bytes(b"authored fake observer identity")

    def observed(self, data, env=None):
        def execute(argv, **kwargs):
            self.argv, self.env = argv, kwargs["env"]
            Path(self.env["SAFE_MEDIA_ALLOCATION_REPORT"]).write_text(json.dumps(data))
            return SimpleNamespace(returncode=0, stdout=b'{"packets":1}\n', stderr=b"")
        with patch.object(memory, "ROOT", self.root), patch.object(memory.platform, "system", return_value="Linux"), \
                patch.dict("os.environ", env or {}, clear=True), patch.object(memory.subprocess, "run", side_effect=execute) as run:
            result = memory.observe("fake-worker", "memory", "original-input", data["mode"], ("raw-arg",))
        return result, run.call_count

    def test_linux_observer_uses_so_and_LD_PRELOAD_without_changing_operation(self):
        result, count = self.observed(receipt())
        self.assertEqual((result["status"], count), ("measured", 1))
        self.assertEqual(self.argv, ["fake-worker", "memory", "original-input", "raw-arg"])
        self.assertEqual(self.env["LD_PRELOAD"], str(self.root / "target/harness/allocations.so"))
        self.assertNotIn("DYLD_INSERT_LIBRARIES", self.env)
        self.assertEqual(result["observer"], receipt())

    def test_inherited_preload_or_audit_is_a_nonpass_before_worker_start(self):
        for name in ("LD_PRELOAD", "LD_AUDIT"):
            with self.subTest(name=name):
                result, count = self.observed(receipt(), {name: "/other/instrument.so"})
                self.assertNotEqual(result["status"], "measured")
                self.assertEqual(count, 0)

    def test_linux_missing_or_inaccurate_RSS_provenance_is_not_measured(self):
        for value in (None, "/proc/self/statm", "/proc/self/status"):
            data = receipt("rss")
            if value is None:
                data.pop("rss_source")
            else:
                data["rss_source"] = value
            result, _ = self.observed(data)
            self.assertEqual(result["status"], "observer-invalid")
            self.assertEqual(result["receipt"], data)

    def test_Hegel_source_RSS_unresolved_protocol_is_retained_and_never_admitted(self):
        data = receipt("rss"); data["status"] = "unresolved-hwm"
        result, count = self.observed(data)
        self.assertEqual(result["status"], "rss-attribution-unresolved")
        self.assertEqual(result["observer"], data)
        self.assertEqual(count, 1)
        forged = receipt("rss"); forged.pop("rss_peak_upper_bound_bytes")
        result, _ = self.observed(forged)
        self.assertEqual(result["status"], "observer-invalid")

    def test_approved_resident_upper_bound_is_explicit_and_does_not_rewrite_HWM(self):
        data = bounded_receipt()
        result, _ = self.observed(data)
        self.assertEqual(result["status"], "measured")
        self.assertEqual(result["observer"], data)
        self.assertNotEqual(data["hwm_end"], data["rss_peak_upper_bound_bytes"])

    def test_unproven_or_inconsistent_resident_ceiling_is_not_admitted(self):
        for key, value in (("rss_peak_upper_bound_basis", "reported-HWM"),
                           ("rss_peak_upper_bound_bytes", 199999),
                           ("rss_peak_upper_bound_bytes", 399999)):
            data = bounded_receipt(); data[key] = value
            with self.subTest(key=key, value=value):
                result, _ = self.observed(data)
                self.assertEqual(result["status"], "observer-invalid")

    def test_resident_bound_formula_never_uses_reference_VmPeak_denominator(self):
        row = {}
        for name in ("candidate", "reference"):
            for mode in ("heap", "rss"):
                data = receipt() if mode == "heap" else bounded_receipt()
                if mode == "rss":
                    data.update(rss_start=1000000, rss_end=1030000, hwm_start=1200000,
                                hwm_end=9000000, virtual_peak_start=1500000)
                    data["rss_peak_observed"] = 1050000 if name == "candidate" else 5000000
                    data["virtual_peak_end"] = 2000000 if name == "candidate" else 1000000000000
                    data["rss_peak_upper_bound_bytes"] = data["virtual_peak_end"]
                row[name + "_" + mode] = dict(status="measured", observer=data,
                    work=dict(packets=1), instrument_sha256="observer")
        calls = []
        def interval(values, seed):
            calls.append(values)
            return {"upper_95": max(a / b for a, b in values)}
        result = memory.evaluate([row], 1, 0, dict(platform="Linux", observer_sha256="observer"), interval)
        self.assertEqual(result["status"], "qualified")
        self.assertEqual(calls[0], [(2000000 - 1000000 + 8192, 5000000 - 1000000 - 8192)])
        self.assertEqual(calls[2], [(2000000, 5000000)])
        self.assertAlmostEqual(result["rss_ratio_upper_95"], 1008192 / 3991808)

    def test_calibration_cannot_qualify_an_unresolved_RSS_observer(self):
        for name in ("memory-check", "candidate", "reference"):
            (self.root / "target/harness" / name).write_bytes(name.encode())
        heap = dict(status="measured", observer=receipt(), work=dict(expected_peak_bytes=123))
        rss_data = receipt("rss"); rss_data["status"] = "unresolved-hwm"
        rss = dict(status="rss-attribution-unresolved", observer=rss_data, receipt=rss_data, work={})
        exports = "\n".join(name + " T" for name in memory.ALLOCATORS | {"write", "writev"})
        linked = dict(status="recorded", interpreter="/authored/ld.so", libraries=[], binding_environment={})
        def nm(argv, **kwargs):
            return exports if "--defined-only" in argv else "malloc@GLIBC_2.17 U\nfree@GLIBC_2.17 U\n"
        with patch.object(memory, "ROOT", self.root), patch.object(memory.platform, "system", return_value="Linux"), \
                patch.dict("os.environ", {}, clear=True), patch.object(memory, "observe", side_effect=[heap, rss]), \
                patch.object(memory, "linux_linked_libraries", return_value=linked), patch.object(memory.subprocess, "check_output", side_effect=nm):
            result = memory.qualification(self.root / "target/harness/candidate", self.root / "target/harness/reference")
        self.assertEqual(result["status"], "unqualified")
        self.assertEqual(result["probe_rss"]["receipt"], rss_data)
        self.assertTrue(any("upper-bound attribution unresolved" in error for error in result["errors"]))

    def test_linux_invalid_units_counts_or_page_size_never_become_measurements(self):
        for key, value in (("page_size", 0), ("heap_peak_bytes", True),
                           ("rss_start", -1), ("hwm_end", 1), ("heap_live_end", None)):
            data = receipt(); data[key] = value
            with self.subTest(key=key):
                result, _ = self.observed(data)
                self.assertEqual(result["status"], "observer-invalid")

    def test_pre_window_unknown_releases_are_preserved_while_active_error_bits_fail(self):
        data = receipt(); data["unknown_releases"] = 2
        result, _ = self.observed(data)
        self.assertEqual(result["status"], "measured")
        self.assertEqual(result["observer"]["unknown_releases"], 2)
        data["error_bits"] = 1
        result, _ = self.observed(data)
        self.assertEqual(result["status"], "observer-invalid")

    def test_GNU_versioned_imports_are_retained_and_not_silently_missed(self):
        symbols = memory.allocator_symbols("malloc@GLIBC_2.17 U\ncalloc@@GLIBC_2.17 U\nfree@GLIBC_2.17 U\n", "Linux")
        self.assertEqual(symbols["allocator_imports"], ["calloc", "free", "malloc"])
        self.assertEqual(symbols["raw_undefined_symbols"], ["malloc@GLIBC_2.17", "calloc@@GLIBC_2.17", "free@GLIBC_2.17"])

    def test_internal_and_extended_allocator_imports_stay_uncovered(self):
        symbols = memory.allocator_symbols("__libc_malloc@GLIBC_PRIVATE U\nstrdup@GLIBC_2.17 U\nreallocarray@GLIBC_2.26 U\nmmap@GLIBC_2.17 U\ncfree@GLIBC_2.17 U\n", "Linux")
        self.assertEqual(symbols["allocator_imports"], ["__libc_malloc", "cfree", "mmap", "reallocarray", "strdup"])
        self.assertEqual(symbols["uncovered"], symbols["allocator_imports"])

    def test_Darwin_allocator_spelling_is_preserved(self):
        data = memory.allocator_symbols("                 _malloc\n                 _free\n                 _strdup\n", "Darwin")
        self.assertEqual(data["allocator_imports"], ["free", "malloc", "strdup"])
        self.assertEqual(data["uncovered"], ["strdup"])

    def test_linux_library_listing_hashes_actual_files_and_preserves_raw_listing(self):
        worker, raw_listing, output = self.main_elf_fixture()
        loader, libc = self.root / "ld-linux-aarch64.so.1", self.root / "libc.so.6"
        listing = raw_listing.split("\n", 1)[1]
        def no_anonymous(argv, **kwargs):
            return listing if "--list" in argv else output(argv, **kwargs)
        with patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "check_output", side_effect=no_anonymous) as command:
            result = memory.linux_linked_libraries(worker)
        self.assertEqual(result["status"], "recorded")
        self.assertEqual(result["dynamic_links"], listing)
        self.assertIn(memory.sha(libc), [row["sha256"] for row in result["libraries"]])
        self.assertEqual(command.call_args_list[1].args[0], [str(loader), "--list", str(worker)])

    def test_unresolved_library_or_unknown_listing_is_a_nonpass(self):
        loader = self.root / "ld.so"; loader.write_bytes(b"authored loader")
        worker = self.root / "worker"; worker.write_bytes(b"authored worker")
        for listing in ("libz.so.1 => not found\n", "unparsed loader output\n"):
            with patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "check_output", side_effect=[f"[Requesting program interpreter: {loader}]", listing]):
                with self.assertRaises(ValueError):
                    memory.linux_linked_libraries(worker)

    def main_elf_fixture(self, *, extra=False, missing=False, duplicate=False, shifted=False, elf_type="DYN"):
        loader, libc, worker = (self.root / name for name in ("ld-linux-aarch64.so.1", "libc.so.6", "worker"))
        for path in (loader, libc, worker): path.write_bytes(("authored " + path.name).encode())
        headers = f"[Requesting program interpreter: {loader}]\nLOAD 0x000000 0x000000 0x000000 0x100 0x100 R E 0x1000\n"
        if shifted: headers = headers.replace("LOAD 0x000000 0x000000", "LOAD 0x000000 0x400000")
        anonymous = "\t (0x0000e73d38b50000)\n"
        listing = anonymous + "linux-vdso.so.1 (0x0000e73d38c7f000)\n" + f"libc.so.6 => {libc} (0x2345)\n{loader} (0x3456)\n"
        if duplicate: listing += anonymous
        if extra:
            other = self.root / "libother.so"; other.write_bytes(b"authored unexpected dependency")
            listing += f"libother.so => {other} (0x4567)\n"
        root = f"ELF Header:\n  Class: ELF64\n  Type: {elf_type} (authored test)\nDynamic section at offset 0x100:\n 0x1 (NEEDED) Shared library: [{'missing.so' if missing else 'libc.so.6'}]\n"
        def output(argv, **kwargs):
            if "--program-headers" in argv: return headers
            if "--list" in argv: return listing
            if "--file-header" in argv: return root
            return "Dynamic section at offset 0x100:\n"
        return worker, listing, output

    def test_parent_observed_leading_anonymous_main_ELF_is_bound_to_full_file_closure(self):
        worker, listing, output = self.main_elf_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "check_output", side_effect=output):
            data = memory.linux_linked_libraries(worker)
        self.assertEqual(data["dynamic_links"], listing)
        self.assertEqual(data["inspected_artifact"]["sha256"], memory.sha(worker))
        self.assertEqual(data["virtual_kernel_libraries"], ["linux-vdso.so.1"])
        self.assertEqual(data["anonymous_main_mapping"]["address_raw"], "0x0000e73d38b50000")
        self.assertEqual(len(data["anonymous_main_mapping"]["needed_closure"]), 3)
        stable = memory.linux_library_identity(data)
        self.assertIn("inspected_main_ELF", stable)
        self.assertNotIn("address_raw", stable["inspected_main_ELF"])

    def test_main_mapping_display_does_not_change_verified_dependency_identity(self):
        worker, listing, output = self.main_elf_fixture()
        def without_anonymous(argv, **kwargs):
            return listing.split("\n", 1)[1] if "--list" in argv else output(argv, **kwargs)
        with patch.dict("os.environ", {}, clear=True):
            with patch.object(memory.subprocess, "check_output", side_effect=output):
                first = memory.linux_linked_libraries(worker)
            with patch.object(memory.subprocess, "check_output", side_effect=without_anonymous):
                second = memory.linux_linked_libraries(worker)
        self.assertNotEqual(first["dynamic_links"], second["dynamic_links"])
        self.assertIsNone(second["anonymous_main_mapping"])
        self.assertEqual(memory.linux_library_identity(first), memory.linux_library_identity(second))

    def test_missing_main_mapping_display_cannot_skip_dependency_closure_validation(self):
        for options in (dict(extra=True), dict(missing=True), dict(shifted=True), dict(elf_type="EXEC")):
            worker, listing, output = self.main_elf_fixture(**options)
            def without_anonymous(argv, **kwargs):
                return listing.split("\n", 1)[1] if "--list" in argv else output(argv, **kwargs)
            with self.subTest(options=options), patch.dict("os.environ", {}, clear=True), \
                    patch.object(memory.subprocess, "check_output", side_effect=without_anonymous):
                with self.assertRaises(ValueError):
                    memory.linux_linked_libraries(worker)

    def test_anonymous_row_cannot_hide_unknown_or_incomplete_file_dependencies(self):
        for options in (dict(extra=True), dict(missing=True), dict(duplicate=True), dict(shifted=True), dict(elf_type="EXEC")):
            worker, _, output = self.main_elf_fixture(**options)
            with self.subTest(options=options), patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "check_output", side_effect=output):
                with self.assertRaises(ValueError): memory.linux_linked_libraries(worker)

    def test_anonymous_plus_vdso_only_is_not_empty_dependency_coverage(self):
        worker, _, output = self.main_elf_fixture()
        def no_files(argv, **kwargs):
            return "\t (0x0000e73d38b50000)\nlinux-vdso.so.1 (0x1234)\n" if "--list" in argv else output(argv, **kwargs)
        with patch.dict("os.environ", {}, clear=True), patch.object(memory.subprocess, "check_output", side_effect=no_files):
            with self.assertRaises(ValueError): memory.linux_linked_libraries(worker)

    def test_stale_v1_receipt_cannot_qualify_as_the_frozen_v2_observer(self):
        data = receipt(); data["observer"] = "linux-glibc-v1"
        result, _ = self.observed(data)
        self.assertEqual(result["status"], "observer-invalid")

    def test_changed_Linux_runtime_libraries_prevent_memory_reuse(self):
        instrumentation = dict(platform="Linux", observer_sha256="observer", probe_sha256="probe",
                               linked_library_hashes={"libc": "old"})
        measurement = dict(status="qualified", pairs=30, certificate=copy.deepcopy(instrumentation))
        self.assertTrue(memory.reusable_memory(measurement, 30, instrumentation))
        changed = {**instrumentation, "linked_library_hashes": {"libc": "changed"}}
        self.assertFalse(memory.reusable_memory(measurement, 30, changed))

    def test_retargeted_library_symlink_invalidates_a_live_certificate(self):
        first, second, alias = (self.root / name for name in ("lib-one", "lib-two", "lib-current"))
        first.write_bytes(b"authored first library"); second.write_bytes(b"authored second library")
        alias.symlink_to(first)
        row = dict(path=str(alias), realpath=str(first.resolve()), sha256=memory.sha(first))
        linked = {name: dict(libraries=[row]) for name in ("candidate", "reference", "probe", "observer")}
        certificate = dict(linked_libraries=linked, linked_library_hashes=memory.linked_hashes(linked), binding_environment={})
        with patch.object(memory, "linux_loader_environment", return_value={}):
            self.assertTrue(memory.linux_certificate_current(certificate))
            alias.unlink(); alias.symlink_to(second)
            self.assertFalse(memory.linux_certificate_current(certificate))

    def test_ASLR_does_not_replace_first_clean_dependency_identity_and_raw_is_retained(self):
        first = dict(status="recorded", libraries=[dict(path="/lib/libc.so.6", sha256="same")],
                     dynamic_links="/lib/libc.so.6 (0x1234)")
        second = {**first, "dynamic_links": "/lib/libc.so.6 (0x5678)"}
        self.assertEqual(memory.linux_library_identity(first), memory.linux_library_identity(second))
        self.assertNotEqual(first["dynamic_links"], second["dynamic_links"])


def proc_stat(pid=123, cpu=10, start=100):
    fields = ["S"] + ["0"] * 20
    fields[11], fields[12], fields[19] = str(cpu), "2", str(start)
    return f"{pid} (clang test) " + " ".join(fields)


class LinuxIsolationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(); self.addCleanup(self.temporary.cleanup)
        self.proc = Path(self.temporary.name)
        (self.proc / "123").mkdir()
        (self.proc / "123/stat").write_text(proc_stat())
        (self.proc / "sys/kernel/random").mkdir(parents=True)
        (self.proc / "sys/kernel/random/boot_id").write_text("authored-boot-id\n")
        (self.proc / "stat").write_text("cpu 10 0 20 100 0 0 0 5 2 1\ncpu0 10 0 20 100 0 0 0 5 2 1\n")

    def snapshot(self, output="123 1-02:03:04 S clang\n"):
        with patch.object(runner.platform, "system", return_value="Linux"), patch.object(runner, "PROC_ROOT", self.proc), \
                patch.object(runner.subprocess, "check_output", return_value=output):
            return runner.cpu_snapshot()

    def test_Linux_ps_days_clock_does_not_replace_native_process_ticks(self):
        snapshot = self.snapshot()
        self.assertEqual(snapshot[123]["cpu_ticks"], 12)
        self.assertEqual(snapshot[123]["ps_clock_raw"], "1-02:03:04")
        self.assertEqual(snapshot["__linux_cpu__"]["status"], "recorded")
        self.assertEqual(snapshot["__linux_cpu__"]["counters"][7], 5)

    def test_process_tick_advance_and_PID_reuse_are_contaminants(self):
        before = self.snapshot()
        for text in (proc_stat(cpu=11), proc_stat(start=101)):
            (self.proc / "123/stat").write_text(text)
            self.assertIn("clang", runner.active_cpu(before, self.snapshot()))

    def test_CPU_steal_is_rejected_without_deleting_raw_counters(self):
        before = self.snapshot("")
        (self.proc / "stat").write_text("cpu 10 0 20 100 0 0 0 6 2 1\ncpu0 10 0 20 100 0 0 0 6 2 1\n")
        after = self.snapshot("")
        self.assertIn("linux-cpu-steal", runner.active_cpu(before, after))
        self.assertEqual(after["__linux_cpu__"]["counters"][7], 6)

    def test_missing_proc_or_failed_ps_is_unavailable_never_idle(self):
        before = self.snapshot("")
        (self.proc / "stat").unlink()
        self.assertIn("linux-cpu-evidence-unavailable", runner.active_cpu(before, self.snapshot("")))
        with patch.object(runner.platform, "system", return_value="Linux"), patch.object(runner, "PROC_ROOT", self.proc), \
                patch.object(runner.subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, "ps")):
            failed = runner.cpu_snapshot()
        self.assertIn("linux-cpu-evidence-unavailable", runner.active_cpu(before, failed))

    def test_missing_process_stat_and_missing_steal_counter_are_not_idle(self):
        before = self.snapshot()
        (self.proc / "123/stat").unlink()
        self.assertIn("linux-cpu-evidence-unavailable", runner.active_cpu(before, self.snapshot()))
        (self.proc / "stat").write_text("cpu 10 0 20 100\n")
        self.assertIn("linux-cpu-evidence-unavailable", runner.active_cpu(before, self.snapshot("")))

    def test_counter_regression_boot_change_and_missing_Linux_marker_fail_closed(self):
        before = self.snapshot("")
        for change in ("steal", "boot"):
            after = copy.deepcopy(before)
            if change == "steal": after["__linux_cpu__"]["counters"][7] -= 1
            else: after["__linux_cpu__"]["boot_id"] = "another-boot"
            self.assertIn("linux-cpu-evidence-invalid", runner.active_cpu(before, after))
        self.assertIn("linux-cpu-evidence-missing", runner.active_cpu({}, {}, require_linux=True))

    def test_Darwin_process_snapshot_shape_and_fractional_clock_are_unchanged(self):
        with patch.object(runner.platform, "system", return_value="Darwin"), \
                patch.object(runner.subprocess, "check_output", return_value="123 1:02.34 S /usr/bin/clang\n"):
            self.assertEqual(runner.cpu_snapshot(), {123: dict(name="clang", cpu_ticks=6234, state="S")})


class LinuxBuildTests(unittest.TestCase):
    def setUp(self):
        path = runner.ROOT / "tools/build-harness.py"
        spec = importlib.util.spec_from_file_location("build_harness_linux_test", path)
        self.build = importlib.util.module_from_spec(spec); spec.loader.exec_module(self.build)

    def test_Linux_build_commands_use_Hegel_sources_so_and_public_link_flags(self):
        commands = self.build.instrumentation_commands("cc", Path("out"), "Linux")
        text = [[str(arg) for arg in command] for command in commands]
        self.assertIn("-shared", text[0]); self.assertIn("-fPIC", text[0]); self.assertIn("-ldl", text[0])
        self.assertIn("-pthread", text[0]); self.assertIn("out/allocations.so", text[0])
        self.assertTrue(any(arg.endswith("harness/allocations-linux.c") for arg in text[0]))
        self.assertTrue(any(arg.endswith("harness/portable-memory-check.c") for arg in text[1]))
        self.assertIn("-fno-builtin", text[1])

    def test_Darwin_commands_keep_existing_sources_dynamiclib_and_probe_flags(self):
        commands = self.build.instrumentation_commands("cc", Path("out"), "Darwin")
        text = [[str(arg) for arg in command] for command in commands]
        self.assertIn("-dynamiclib", text[0]); self.assertIn("out/allocations.dylib", text[0])
        self.assertTrue(any(arg.endswith("harness/allocations.c") for arg in text[0]))
        self.assertTrue(any(arg.endswith("harness/memory-check.c") for arg in text[1]))

    def test_Linux_link_identity_cannot_be_a_platform_skip(self):
        data = {"status": "recorded", "libraries": [{"path": "/authored/lib.so", "sha256": "hash"}]}
        with patch.object(self.build.platform, "system", return_value="Linux"), \
                patch.object(self.build, "linux_linked_libraries", return_value=data):
            self.assertEqual(self.build.system_libraries(Path("worker")), data)


if __name__ == "__main__":
    unittest.main()
