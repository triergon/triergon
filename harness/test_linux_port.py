"""Linux timing-isolation controls using authored /proc data; no native process starts."""
import copy
import json
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from . import runner


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
