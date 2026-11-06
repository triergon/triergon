"""H0 integrity regressions: no real workers, native tools or measurements.

All subprocess and signal boundaries are mocked. CLI cases invoke the real
entry point with authored inputs, a mocked mutation generator and mocked workers.
"""
import contextlib
from decimal import Decimal
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from tools import fuzz_media
from . import fuzz
from .common import digest_bytes
from .runner import ROOT


def ok(data):
    return {"status": "ok", "data": data}


def nested(depth):
    value = 0
    for _ in range(depth):
        value = [value]
    return value


class ExactJsonIntegrityTests(unittest.TestCase):
    def test_fractional_tokens_are_decimal_without_rounding(self):
        for token in ("0.1000000000000000000000000001", "9007199254740993.0",
                      "1e-400", "1.7976931348623157e308"):
            with self.subTest(token=token):
                actual = fuzz.strict_json(token.encode())
                self.assertIsInstance(actual, Decimal)
                self.assertEqual(actual, Decimal(token))

    def test_fractional_overflow_is_rejected_including_finite_decimal_tokens(self):
        for token in ("1e400", "1e401", "-1e400", "1.7976931348623159e308"):
            with self.subTest(token=token), self.assertRaises(ValueError):
                fuzz.strict_json(('{"n":' + token + '}').encode())

    def test_distinct_decimals_are_findings_with_serializable_exact_tags(self):
        pairs = (("9007199254740992.0", "9007199254740993.0"),
                 ("0.1000000000000000000000000001", "0.1000000000000000000000000002"))
        for left, right in pairs:
            with self.subTest(left=left, right=right):
                result = fuzz.compare({"candidate": ok(fuzz.strict_json(left)),
                                       "reference": ok(fuzz.strict_json(right))})
                self.assertFalse(result["qualification"])
                self.assertEqual(result["comparisons"][0]["status"], "different")
                delta = result["findings"][0]["differences"][0]
                for key, wanted in (("actual", left), ("expected", right)):
                    self.assertIsInstance(delta[key], dict)
                    self.assertEqual(set(delta[key]), {"json_decimal"})
                    self.assertIsInstance(delta[key]["json_decimal"], str)
                    self.assertEqual(Decimal(delta[key]["json_decimal"]), Decimal(wanted))
                json.dumps(result, allow_nan=False)

    def test_decimal_tags_remain_serializable_in_container_differences(self):
        left = {"value": [Decimal("0.125")]}
        right = {"value": {"entry": Decimal("0.25")}}
        result = fuzz.compare({"candidate": ok(left), "reference": ok(right)})
        wire = json.dumps(result, allow_nan=False)
        self.assertIn('"json_decimal"', wire)
        self.assertTrue(result["findings"])

    def test_long_decimal_differences_keep_exact_values_across_summary_boundary(self):
        for length in (255, 256, 302):
            left = "1." + "1" * (length - 2)
            right = left[:-1] + "2"
            with self.subTest(length=length):
                result = fuzz.compare({"candidate": ok(Decimal(left)),
                                       "reference": ok(Decimal(right))})
                wire = json.dumps(result, allow_nan=False)
                self.assertIn(left, wire)
                self.assertIn(right, wire)

    def test_long_decimals_in_mismatched_containers_keep_exact_tags(self):
        left = "1." + "1" * 300
        right = left[:-1] + "2"
        result = fuzz.compare({"candidate": ok([Decimal(left)]),
                               "reference": ok({"value": Decimal(right)})})
        wire = json.dumps(result, allow_nan=False)
        self.assertIn(left, wire)
        self.assertIn(right, wire)
        self.assertIn('"json_decimal"', wire)

    def test_integer_decimal_exact_equality_and_boolean_distinction(self):
        for number in (0, -7, 2 ** 80 + 1):
            with self.subTest(number=number):
                result = fuzz.compare({"candidate": ok(number),
                                       "reference": ok(Decimal(str(number) + ".0"))})
                self.assertEqual(result["findings"], [])
                self.assertEqual(result["comparisons"][0]["status"], "observed-agreement")
                self.assertFalse(result["qualification"])
        self.assertTrue(fuzz.compare({"candidate": ok(True),
                                      "reference": ok(Decimal(1))})["findings"])
        self.assertTrue(fuzz.compare({"candidate": ok(0),
                                      "reference": ok(Decimal("1e-400"))})["findings"])

    def test_integer_tokens_remain_exact_and_duplicates_constants_reject(self):
        value = 2 ** 127 + 1
        self.assertIs(type(fuzz.strict_json(str(value))), int)
        self.assertEqual(fuzz.strict_json(str(value)), value)
        for wire in ('{"n":1,"n":2}', '{"n":NaN}', '{"n":Infinity}', '{"n":-Infinity}'):
            with self.subTest(wire=wire), self.assertRaises(ValueError):
                fuzz.strict_json(wire)

    def test_protocol_nesting_boundary_is_256(self):
        wire = "[" * 256 + "0" + "]" * 256
        self.assertIsInstance(fuzz.strict_json(wire), list)
        with self.assertRaises(ValueError):
            fuzz.strict_json("[" * 257 + "0" + "]" * 257)

    def test_direct_deep_comparison_is_a_retained_limit_finding(self):
        value = nested(1050)
        try:
            result = fuzz.compare({"candidate": ok(value), "reference": ok(value)})
        except Exception as error:
            self.fail("deep comparison escaped instead of retaining a limit: " + repr(error))
        self.assertFalse(result["qualification"])
        self.assertTrue(any(row.get("kind") == "comparison-limit"
                            for row in result["findings"]), result)
        self.assertFalse(any(row.get("status") == "observed-agreement"
                             for row in result["comparisons"]))
        json.dumps(result, allow_nan=False)

    def test_shallow_values_order_and_ungraded_rejections_are_controls(self):
        same = fuzz.compare({"candidate": ok({"p": [None, False, 3]}),
                             "reference": ok({"p": [None, False, 3]})})
        self.assertEqual(same["findings"], [])
        changed = fuzz.compare({"candidate": ok([1, 2]), "reference": ok([2, 1])})
        self.assertTrue(changed["findings"])
        rejected = {"status": "rejected", "returncode": 1, "stderr_sha256": "same"}
        result = fuzz.compare({"candidate": dict(rejected), "reference": dict(rejected)})
        self.assertEqual(result["comparisons"][0]["status"], "both-rejected-ungraded")
        self.assertFalse(result["qualification"])


class _Pipe:
    def __init__(self, number):
        self.number, self.closed = number, False

    def fileno(self):
        return self.number

    def close(self):
        self.closed = True


class _Process:
    """No OS process exists: the leader and group are independent model state."""
    def __init__(self, returncode=0, group_exists=True):
        self.pid = 33001
        self.stdout, self.stderr = _Pipe(101), _Pipe(102)
        self.returncode = returncode
        self.group_exists = group_exists
        self.group_alive = group_exists

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        if self.returncode is None:
            raise subprocess.TimeoutExpired("mock-only", timeout)
        return self.returncode

    def kill_group(self, pid, sig):
        if pid != self.pid:
            raise AssertionError("cleanup targeted an unrelated process group")
        if not self.group_exists:
            raise ProcessLookupError("mock group is already absent")
        if sig in (signal.SIGKILL, signal.SIGTERM):
            self.group_alive = False
            if self.returncode is None:
                self.returncode = -sig


class _Selector:
    def __init__(self):
        self.entries = {}

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        return None

    def register(self, pipe, events, label):
        self.entries[pipe.number] = SimpleNamespace(fileobj=pipe, data=label)

    def unregister(self, pipe):
        del self.entries[pipe.number]

    def get_map(self):
        return self.entries

    def select(self, timeout=None):
        return [(entry, 1) for entry in self.entries.values()]


def observe_mocked(payload, *, returncode=0, group_exists=True,
                   maximum=16384, expired=False, read_error=None):
    process = _Process(returncode, group_exists)
    streams = {101: io.BytesIO(payload), 102: io.BytesIO(b"")}

    def read(number, count):
        if read_error:
            raise read_error
        return streams[number].read(count)

    clock = Mock(side_effect=[0.0] + [2.0] * 30) if expired else Mock(return_value=0.0)
    with contextlib.ExitStack() as stack:
        popen = stack.enter_context(patch.object(fuzz.subprocess, "Popen", return_value=process))
        killed = stack.enter_context(patch.object(fuzz.os, "killpg", side_effect=process.kill_group))
        stack.enter_context(patch.object(fuzz.os, "getpgid", return_value=process.pid))
        stack.enter_context(patch.object(fuzz.os, "set_blocking"))
        stack.enter_context(patch.object(fuzz.os, "read", side_effect=read))
        stack.enter_context(patch.object(fuzz.selectors, "DefaultSelector", _Selector))
        stack.enter_context(patch.object(fuzz.time, "monotonic", clock))
        try:
            outcome = fuzz.observe(["MOCK-WORKER-NEVER-EXECUTED"], 1.0, maximum)
        except Exception as error:
            return SimpleNamespace(process=process, popen=popen, killed=killed, error=error)
    return SimpleNamespace(process=process, popen=popen, killed=killed,
                           error=None, outcome=outcome)


@unittest.skipUnless(os.name == "posix", "the watchdog currently requires POSIX")
class ProcessGroupIntegrityTests(unittest.TestCase):
    def test_completed_leader_does_not_leave_its_owned_group_alive(self):
        run = observe_mocked(b'{"value":1}')
        self.assertIsNone(run.error)
        self.assertEqual(run.outcome[0]["status"], "ok")
        self.assertFalse(run.process.group_alive, "leader exited but its descendant was left alive")
        self.assertTrue(run.killed.called)
        self.assertTrue(all(call.args[0] == run.process.pid for call in run.killed.call_args_list))
        self.assertTrue(run.popen.call_args.kwargs["start_new_session"])

    def test_exception_also_cleans_group_after_leader_exit(self):
        run = observe_mocked(b"", read_error=OSError("authored read failure"))
        self.assertIsInstance(run.error, OSError)
        self.assertFalse(run.process.group_alive)
        self.assertTrue(run.process.stdout.closed)
        self.assertTrue(run.process.stderr.closed)

    def test_absent_group_is_a_normal_cleanup_control(self):
        run = observe_mocked(b'{"value":1}', group_exists=False)
        self.assertIsNone(run.error)
        self.assertEqual(run.outcome[0]["status"], "ok")
        self.assertEqual(run.outcome[1], b'{"value":1}')

    def test_timeout_and_output_limit_only_terminate_the_owned_group(self):
        for options, status in (({"payload": b"", "returncode": None, "expired": True}, "timeout"),
                                ({"payload": b"x" * 1025, "returncode": None, "maximum": 1024}, "output-limit")):
            with self.subTest(status=status):
                run = observe_mocked(**options)
                self.assertIsNone(run.error)
                self.assertEqual(run.outcome[0]["status"], status)
                self.assertFalse(run.process.group_alive)
                self.assertTrue(run.killed.called)
                self.assertTrue(all(call.args[0] == run.process.pid
                                    for call in run.killed.call_args_list))
                self.assertLessEqual(len(run.outcome[1]) + len(run.outcome[2]),
                                     options.get("maximum", 16384))


class CliIntegrityTests(unittest.TestCase):
    @contextlib.contextmanager
    def fixture(self):
        private = ROOT / ".research/h0-fuzz-gate-regressions-v1"
        private.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="mocked-cli-", dir=private) as temporary:
            directory = Path(temporary)
            seed = directory / "seed.bin"
            seed.write_bytes(b"original")
            source = directory / "source-pin.txt"
            source.write_bytes(b"authored source fixture")
            workers = {}
            for role in ("candidate", "reference"):
                path = directory / (role + ".inert")
                path.write_bytes(("not executable: " + role).encode())
                workers[role] = {"path": str(path), "sha256": digest_bytes(path.read_bytes())}
            manifest = {
                "schema": 1, "workers": workers,
                "source_files": {str(source): digest_bytes(source.read_bytes())},
                "operations": [{"id": "trace", "roles": {
                    role: ["trace", "{input}"] for role in workers}}],
                "seeds": [{"id": "seed", "path": str(seed),
                           "sha256": digest_bytes(seed.read_bytes()), "rights": "authored-synthetic",
                           "baseline_statuses": {"trace": dict.fromkeys(workers, "ok")}}],
            }
            manifest_path = directory / "manifest.json"
            initial = (json.dumps(manifest, indent=2) + "\n").encode()
            manifest_path.write_bytes(initial)
            fixture = SimpleNamespace(cli=fuzz_media, root=directory, output=directory / "output",
                                      source=source, seed=seed, manifest=manifest,
                                      manifest_path=manifest_path, initial_manifest=initial,
                                      calls=[], payload=b'{"length":8}')
            yield fixture

    def run_cli(self, fixture, *, before_source_hash=None, tamper=False, deep=False):
        cli = fixture.cli
        actual_sha = cli.sha
        changed = False

        def sha(path):
            nonlocal changed
            value = actual_sha(path)
            if before_source_hash and Path(path).resolve() == fixture.source.resolve() and not changed:
                changed = True
                before_source_hash()
            return value

        def mutation(seed, rng, maximum):
            # Authored fixture output, not the production mutation generator.
            data = seed + b"\0"
            return data, {"schema": 1, "seed_sha256": digest_bytes(seed),
                          "result_sha256": digest_bytes(data),
                          "edits": [{"offset": len(seed), "remove": 0, "insert_hex": "00"}]}

        def observe(command, timeout, maximum):
            path = Path(command[-1])
            role = Path(command[0]).stem
            fixture.calls.append({"role": role, "command": list(command),
                                  "input": path.read_bytes(), "path": str(path)})
            if deep:
                raw = ("[" * 1050 + "0" + "]" * 1050).encode()
                observed = observe_mocked(raw, group_exists=False, maximum=maximum)
                if observed.error:
                    raise observed.error
                return observed.outcome
            raw = json.dumps({"length": len(path.read_bytes())}, separators=(",", ":")).encode()
            result = {"status": "ok", "returncode": 0, "command": list(command),
                      "stdout_sha256": digest_bytes(raw), "stderr_sha256": digest_bytes(b""),
                      "output_truncated": False, "output_bytes_seen": len(raw),
                      "elapsed_seconds": 0.0, "data": json.loads(raw)}
            if tamper and len(fixture.calls) == 1:
                path.write_bytes(b"tampered")
            return result, raw, b""

        argv = ["fuzz_media.py", "--manifest", str(fixture.manifest_path),
                "--output", str(fixture.output), "--rounds", "1",
                "--maximum-output-bytes", "16384"]
        with contextlib.ExitStack() as stack:
            forbidden = stack.enter_context(patch.object(
                subprocess, "Popen", side_effect=AssertionError("a real worker must never start")))
            stack.enter_context(patch.object(cli, "sha", side_effect=sha))
            stack.enter_context(patch.object(cli, "mutation", side_effect=mutation))
            stack.enter_context(patch.object(cli, "observe", side_effect=observe))
            stack.enter_context(patch.object(cli.platform, "platform", return_value="mock-platform"))
            stack.enter_context(patch.object(cli.platform, "machine", return_value="mock-machine"))
            stack.enter_context(patch.object(sys, "argv", argv))
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
            try:
                code, error = cli.main(), None
            except SystemExit as caught:
                code, error = caught.code, caught
            except Exception as caught:
                code, error = None, caught
            forbidden.assert_not_called()
        report_path = fixture.output / "report.json"
        report = json.loads(report_path.read_text()) if report_path.exists() else None
        return SimpleNamespace(code=code, error=error, report=report, source_hook_fired=changed)

    def test_unchanged_cli_control_keeps_replay_raw_streams_and_remains_unqualified(self):
        with self.fixture() as fixture:
            run = self.run_cli(fixture)
            self.assertIsNone(run.error)
            self.assertEqual(run.code, 0)
            self.assertEqual(run.report["collection_status"], "complete")
            self.assertEqual(run.report["findings_count"], 0)
            self.assertFalse(run.report["qualification"])
            self.assertEqual(len(fixture.calls), 4)
            identity = json.loads((fixture.output / "identity.json").read_text())
            self.assertEqual(identity["manifest_sha256"], digest_bytes(fixture.initial_manifest))
            self.assertEqual((fixture.output / "manifest.json").read_bytes(), fixture.initial_manifest)
            for case in sorted(fixture.output.glob("case-*")):
                observation = json.loads((case / "observation.json").read_text())
                self.assertEqual(observation["input_sha256"], digest_bytes((case / "input.bin").read_bytes()))
                self.assertFalse(observation["qualification"])
                for role in ("candidate", "reference"):
                    self.assertTrue((case / "trace" / (role + ".stdout")).exists())
                    self.assertTrue((case / "trace" / (role + ".stderr")).exists())

    def test_manifest_replacement_cannot_publish_identity_for_different_executed_bytes(self):
        with self.fixture() as fixture:
            def replace():
                replacement = json.loads(fixture.initial_manifest)
                for arguments in replacement["operations"][0]["roles"].values():
                    arguments[0] = "metadata-trace"
                fixture.manifest_path.write_text(json.dumps(replacement) + "\n")
            run = self.run_cli(fixture, before_source_hash=replace)
            self.assertTrue(run.source_hook_fired, "replacement must happen after parse, during preflight")
            stable = bool(run.report and run.report.get("collection_status") == "complete"
                          and run.report.get("source_workers_and_seeds_unchanged"))
            if run.error is not None or run.code != 0:
                self.assertFalse(stable, "changed manifest still published a stable completed collection")
            else:
                identity = json.loads((fixture.output / "identity.json").read_text())
                self.assertEqual(identity["manifest_sha256"], digest_bytes(fixture.initial_manifest))
                self.assertEqual((fixture.output / "manifest.json").read_bytes(), fixture.initial_manifest)
                self.assertTrue(all(row["command"][1] == "trace" for row in fixture.calls))

    def test_input_tampering_is_structured_nonpass_without_cross_input_agreement(self):
        with self.fixture() as fixture:
            run = self.run_cli(fixture, tamper=True)
            self.assertIsNone(run.error, "input tampering must retain a structured case result")
            self.assertNotEqual(run.code, 0)
            observation_path = fixture.output / "case-000000/observation.json"
            self.assertTrue(observation_path.exists())
            case = json.loads(observation_path.read_text())
            self.assertFalse(case["qualification"])
            operations = case["operations"]
            self.assertTrue(any(row.get("findings") for row in operations))
            self.assertFalse(any(comparison.get("status") == "observed-agreement"
                                 for row in operations for comparison in row.get("comparisons", [])))
            raw = fixture.output / "case-000000/trace/candidate.stdout"
            self.assertTrue(raw.exists(), "captured worker stdout must survive input rejection")
            self.assertEqual(fixture.seed.read_bytes(), b"original")
            originals = [path for path in fixture.output.rglob("*")
                         if path.is_file() and path.read_bytes() == b"original"]
            self.assertTrue(originals, "the collection must preserve an original source-byte copy")

    @unittest.skipUnless(os.name == "posix", "the watchdog currently requires POSIX")
    def test_deep_json_keeps_cli_case_and_raw_streams_as_explicit_findings(self):
        with self.fixture() as fixture:
            run = self.run_cli(fixture, deep=True)
            self.assertIsNone(run.error, "deep JSON must not abort the case or collection")
            self.assertNotEqual(run.code, 0)
            self.assertIsNotNone(run.report)
            self.assertFalse(run.report["qualification"])
            self.assertGreater(run.report["findings_count"], 0)
            case_path = fixture.output / "case-000000"
            case = json.loads((case_path / "observation.json").read_text())
            findings = [item for row in case["operations"] for item in row["findings"]]
            self.assertTrue(any(item.get("kind") in ("worker-invalid-json", "invalid-json", "comparison-limit")
                                for item in findings), findings)
            self.assertTrue((case_path / "trace/candidate.stdout").exists())
            self.assertEqual((case_path / "trace/candidate.stdout").read_bytes(),
                             ("[" * 1050 + "0" + "]" * 1050).encode())
            self.assertTrue((fixture.output / "progress.json").exists())


if __name__ == "__main__":
    unittest.main()
