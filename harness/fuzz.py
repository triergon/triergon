"""Reproducible byte mutations and bounded, ungraded worker observations.

These helpers never grant feature, speed, memory or complete-module acceptance.
"""
from __future__ import annotations

import hashlib
from decimal import Decimal, InvalidOperation
import math
import json
import os
from pathlib import Path
import random
import selectors
import signal
import subprocess
import time

from .runner import differences


def sha_bytes(data):
    return hashlib.sha256(data).hexdigest()


MAX_JSON_DEPTH = 256
MAX_JSON_NODES = 1_000_000


def validate_json_shape(data):
    """Bound traversal with an iterator stack, independently of Python recursion."""
    stack, nodes = [(iter((data,)), 0)], 0
    while stack:
        values, depth = stack[-1]
        try:
            value = next(values)
        except StopIteration:
            stack.pop()
            continue
        nodes += 1
        if nodes > MAX_JSON_NODES:
            raise ValueError("JSON node limit exceeded")
        if isinstance(value, (dict, list)):
            if depth >= MAX_JSON_DEPTH:
                raise ValueError("JSON nesting limit exceeded")
            if isinstance(value, dict) and not all(isinstance(key, str) for key in value):
                raise ValueError("JSON object key is not text")
            stack.append((iter(value.values() if isinstance(value, dict) else value), depth + 1))
        elif isinstance(value, (float, Decimal)):
            if not math.isfinite(float(value)):
                raise ValueError("JSON number exceeds the finite binary64 protocol range")
        elif value is not None and not isinstance(value, (str, int, bool)):
            raise ValueError("non-JSON comparison value")


def json_decimal(value):
    """Keep exact differing decimal values serializable without rounding them."""
    if isinstance(value, Decimal):
        return {"json_decimal": str(value)}
    raise TypeError("unsupported JSON result value: " + type(value).__name__)


def difference_value(value):
    # A structural mismatch can contain decimals below its root. Preserve that
    # bounded subtree until json_decimal tags every exact number it contains.
    if isinstance(value, (Decimal, dict, list)):
        return value
    return value if len(str(value)) < 256 else "<large value>"


def strict_json(data):
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON field: " + key)
            result[key] = value
        return result

    def constant(value):
        raise ValueError("non-finite JSON constant: " + value)

    def decimal(value):
        if not math.isfinite(float(value)):
            raise ValueError("JSON number exceeds the finite binary64 protocol range")
        try:
            return Decimal(value)
        except InvalidOperation as error:
            raise ValueError("invalid JSON decimal") from error

    try:
        result = json.loads(data, object_pairs_hook=object_pairs,
                            parse_constant=constant, parse_float=decimal)
    except RecursionError as error:
        raise ValueError("JSON nesting limit exceeded") from error
    validate_json_shape(result)
    return result


def replay(seed, recipe, maximum_bytes):
    if type(recipe.get("schema")) is not int or recipe["schema"] != 1 or recipe.get("seed_sha256") != sha_bytes(seed):
        raise ValueError("mutation recipe does not bind this seed")
    if type(maximum_bytes) is not int or maximum_bytes < len(seed) or maximum_bytes < 1:
        raise ValueError("invalid mutation input bound")
    edits = recipe.get("edits")
    if not isinstance(edits, list) or not 1 <= len(edits) <= 8:
        raise ValueError("need one to eight bounded edits")
    result = bytes(seed)
    for edit in edits:
        if set(edit) != {"offset", "remove", "insert_hex"}:
            raise ValueError("unknown mutation edit fields")
        offset, remove, encoded = edit["offset"], edit["remove"], edit["insert_hex"]
        if (type(offset) is not int or type(remove) is not int or
                not 0 <= offset <= len(result) or not 0 <= remove <= len(result) - offset):
            raise ValueError("mutation edit is outside its current input")
        if not isinstance(encoded, str) or len(encoded) % 2 or len(encoded) > 2 * maximum_bytes:
            raise ValueError("invalid mutation bytes")
        if any(c not in "0123456789abcdefABCDEF" for c in encoded):
            raise ValueError("invalid mutation hexadecimal data")
        if len(result) - remove + len(encoded) // 2 > maximum_bytes:
            raise ValueError("mutation exceeds its declared input bound")
        result = result[:offset] + bytes.fromhex(encoded) + result[offset + remove:]
    if recipe.get("result_sha256") != sha_bytes(result):
        raise ValueError("mutation result hash disagrees with its recipe")
    return result


def mutation(seed, rng: random.Random, maximum_bytes, maximum_edits=4):
    if not 1 <= maximum_edits <= 7 or not 0 <= len(seed) <= maximum_bytes or maximum_bytes < 1:
        raise ValueError("invalid mutation generation bounds")
    data, edits = bytes(seed), []

    def apply(offset, remove, inserted):
        nonlocal data
        edits.append({"offset": offset, "remove": remove, "insert_hex": inserted.hex()})
        data = data[:offset] + inserted + data[offset + remove:]

    for _ in range(rng.randint(1, maximum_edits)):
        choices = ["insert"] if not data else ["flip", "delete", "truncate", "overwrite", "duplicate"]
        if len(data) < maximum_bytes and data:
            choices.append("insert")
        kind = rng.choice(choices)
        if kind == "flip":
            offset = rng.randrange(len(data))
            apply(offset, 1, bytes([data[offset] ^ (1 << rng.randrange(8))]))
        elif kind == "delete":
            offset = rng.randrange(len(data))
            apply(offset, rng.randint(1, min(4096, len(data) - offset)), b"")
        elif kind == "truncate":
            offset = rng.randrange(len(data))
            apply(offset, len(data) - offset, b"")
        elif kind == "overwrite":
            offset = rng.randrange(len(data))
            count = rng.randint(1, min(8, len(data) - offset))
            # Exercise VINT markers, unknown lengths, signed bounds and zeros.
            value = rng.choice([0, 0x7f, 0x80, 0xff])
            apply(offset, count, bytes([value]) * count)
        elif kind == "duplicate" and len(data) < maximum_bytes:
            start = rng.randrange(len(data))
            count = rng.randint(1, min(4096, len(data) - start, maximum_bytes - len(data)))
            apply(rng.randrange(len(data) + 1), 0, data[start:start + count])
        else:
            capacity = min(16, maximum_bytes - len(data))
            if not capacity:
                offset = rng.randrange(len(data))
                apply(offset, 1, bytes([data[offset] ^ 0x80]))
            else:
                count = rng.randint(1, capacity)
                apply(rng.randrange(len(data) + 1), 0, bytes(rng.randrange(256) for _ in range(count)))
    if data == seed:
        if data:
            apply(0, 1, bytes([data[0] ^ 1]))
        else:
            apply(0, 0, b"\x80")
    recipe = {"schema": 1, "seed_sha256": sha_bytes(seed), "edits": edits,
              "result_sha256": sha_bytes(data)}
    if replay(seed, recipe, maximum_bytes) != data:
        raise AssertionError("generated mutation is not reproducible")
    return data, recipe


def observe(command, timeout_seconds, maximum_output_bytes):
    """Capture both pipes; kill only this worker's process group on a limit.

    The input, output and elapsed-time limits are controller bounds. This is not
    an OS memory sandbox, a peak-memory measurement or a timing observation.
    """
    if os.name != "posix":
        raise ValueError("this worker watchdog currently requires a POSIX host")
    if timeout_seconds <= 0 or maximum_output_bytes < 1:
        raise ValueError("invalid worker observation bounds")
    started = time.monotonic()
    process = subprocess.Popen(list(map(str, command)), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               stdin=subprocess.DEVNULL, start_new_session=True)
    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    state, truncated, seen = "completed", False, 0
    try:
        with selectors.DefaultSelector() as selector:
            for label, pipe in (("stdout", process.stdout), ("stderr", process.stderr)):
                os.set_blocking(pipe.fileno(), False)
                selector.register(pipe, selectors.EVENT_READ, label)
            while selector.get_map():
                remaining = timeout_seconds - (time.monotonic() - started)
                if remaining <= 0:
                    state = "timeout"
                    break
                for key, _ in selector.select(min(remaining, .05)):
                    block = os.read(key.fileobj.fileno(), 65536)
                    if not block:
                        selector.unregister(key.fileobj)
                        continue
                    available = max(0, maximum_output_bytes - seen)
                    buffers[key.data].extend(block[:available])
                    seen += len(block)
                    if seen > maximum_output_bytes:
                        state, truncated = "output-limit", True
                        break
                if state != "completed":
                    break
        if state != "completed":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        remaining = max(.001, timeout_seconds - (time.monotonic() - started))
        try:
            code = process.wait(timeout=remaining if state == "completed" else 5)
        except subprocess.TimeoutExpired:
            state = "timeout"
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            code = process.wait()
    finally:
        process.stdout.close()
        process.stderr.close()
        # The leader may already be reaped while same-group descendants still
        # hold resources. This group was created by start_new_session above.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        if process.poll() is None:
            process.wait()
    stdout, stderr = bytes(buffers["stdout"]), bytes(buffers["stderr"])
    result = {"status": state, "returncode": code, "command": list(map(str, command)),
              "stdout_sha256": sha_bytes(stdout), "stderr_sha256": sha_bytes(stderr),
              "output_truncated": truncated, "output_bytes_seen": seen,
              "elapsed_seconds": time.monotonic() - started}
    if state == "completed":
        if code < 0 or code == 101:
            result["status"] = "crash"
        elif code:
            result["status"] = "rejected"
        else:
            try:
                result["data"] = strict_json(stdout)
                result["status"] = "ok"
            except (ValueError, UnicodeError) as error:
                result["status"], result["protocol_error"] = "invalid-json", str(error)
    return result, stdout, stderr


def compare(outcomes):
    """Keep rejection and incomplete observations ungraded, never accepted."""
    if "candidate" not in outcomes or len(outcomes) < 2:
        raise ValueError("a differential observation needs candidate and another role")
    findings, comparisons = [], []
    for role, result in outcomes.items():
        if result["status"] not in ("ok", "rejected"):
            findings.append({"kind": "worker-" + result["status"], "role": role})
    for role in sorted(set(outcomes) - {"candidate"}):
        left, right = outcomes["candidate"], outcomes[role]
        if left["status"] == right["status"] == "ok":
            try:
                validate_json_shape(left["data"])
                validate_json_shape(right["data"])
                delta = differences(left["data"], right["data"], display_value=difference_value)
                delta = json.loads(json.dumps(delta, default=json_decimal, allow_nan=False))
            except (ValueError, RecursionError) as error:
                comparisons.append({"role": role, "status": "comparison-limit", "reason": str(error)})
                findings.append({"kind": "comparison-limit", "role": role, "reason": str(error)})
                continue
            comparisons.append({"role": role, "status": "different" if delta else "observed-agreement", "differences": delta})
            if delta:
                findings.append({"kind": "output-disagreement", "role": role, "differences": delta})
        elif left["status"] == right["status"] == "rejected":
            row = {"role": role, "status": "both-rejected-ungraded"}
            if role == "debug" and (left["returncode"], left["stderr_sha256"]) != (right["returncode"], right["stderr_sha256"]):
                findings.append({"kind": "debug-rejection-disagreement", "role": role})
            comparisons.append(row)
        else:
            comparisons.append({"role": role, "status": "not-equivalent", "candidate": left["status"], "other": right["status"]})
            if {left["status"], right["status"]} <= {"ok", "rejected"}:
                findings.append({"kind": "acceptance-disagreement", "role": role})
    return {"comparisons": comparisons, "findings": findings, "qualification": False}
