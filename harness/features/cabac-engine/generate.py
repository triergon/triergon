"""Author CABAC scripts and their target-specific oracle pins."""
from hashlib import sha256
import json
from pathlib import Path

from harness.cabac_oracle import Encoder, initialized, packed, trace


def mixed():
    encoder = Encoder()
    calls = []
    init = [(20, -15, -1), (-12, 72, 37), (36, -14, 77), (0, 63, 0), (0, 64, 0)]
    for slot, parameters in enumerate(init):
        calls.append("init " + " ".join(map(str, (slot, *parameters))))
        state = initialized(*parameters)
        for step in range(16):
            bin_value = (step ^ (step >> 2) ^ slot) & 1
            state = encoder.decision(state, bin_value)
            state = encoder.decision(state, bin_value ^ 1)
            calls.extend(("refill", f"unfilled {slot}", f"decision {slot}"))
        encoder.terminate(0)
        calls.append("terminate")
    # Exercise both bin values and probability transitions at all 63 states
    # reachable from the specification's context initialization.
    for probability in range(63):
        for mps in range(2):
            for bin_value in range(2):
                state = probability, mps
                encoder.decision(state, bin_value)
                calls.extend((f"state 0 {packed(state)}", "decision 0"))
    encoder.terminate(1)
    calls.append("terminate")
    first = encoder.bytes()
    encoder = Encoder()
    encoder.bypass("1")
    encoder.terminate(1)
    return b"\x99" + first + encoder.bytes(), [
        "new 1", "data", *calls, f"restart {1 + len(first)}", "bypass", "terminate",
    ]


def widths():
    encoder = Encoder()
    calls = ["new 0", "bits 0"]
    for width in range(1, 33):
        sequence = "".join(str((bit ^ (bit // 3) ^ width) & 1) for bit in range(width))
        encoder.bypass(sequence)
        calls.append(f"bits {width}")
    encoder.bypass("1010110110001011")
    calls.extend(("refill", "peek 16", "peek 16", "consume 9", "refill", "bits 7"))
    encoder.terminate(1)
    return encoder.bytes(), [*calls, "terminate", "data"]


def unary(limit, prefix):
    encoder = Encoder()
    encoder.bypass(prefix + "00101010")
    return encoder.bytes(), ["new 0", f"unary {limit}", "bits 8", "data"]


def write():
    directory = Path(__file__).resolve().parent
    inputs = directory / "inputs"
    cases = []
    sources = {
        "mixed": mixed(),
        "bypass-widths": widths(),
        "unary-stop": unary(31, "1" * 13 + "0"),
        "unary-boundary": unary(8, "1" * 8),
        "unary-cap": unary(3, "1" * 8),
        "unary-early-zero-past-cap": unary(3, "111110"),
        "refusals": (
            bytes.fromhex("ff00ff80fe80"),
            ["new 0", "new 2", "new 4", "restart 0", "restart 2", "restart 8", "bits 32", "data"],
        ),
        "empty-input": (b"", ["new 0", "data", "bits 32", "terminate"]),
    }
    for name, (data, calls) in sources.items():
        path = inputs / f"{name}.case"
        path.write_text("\n".join([data.hex(), *calls, ""]))
        for layout in ("plain", "headroom"):
            pins = {}
            for target, width in (
                ("aarch64-unknown-linux-gnu", 64),
                ("thumbv7neon-unknown-linux-gnueabihf", 32),
            ):
                rows = trace(path, layout, width)
                pins[target] = {"trace_sha256": sha256(("\n".join(rows) + "\n").encode()).hexdigest()}
            cases.append({
                "id": f"{name}-{layout}", "profile": layout, "file": path.name,
                "sha256": sha256(path.read_bytes()).hexdigest(), "rights": "authored-synthetic",
                "arguments": [layout], "expected_by_target": pins,
            })
    (directory / "corpus.json").write_text(json.dumps({
        "schema": 1, "directory": "harness/features/cabac-engine/inputs", "cases": cases,
    }, indent=2) + "\n")


if __name__ == "__main__":
    write()
