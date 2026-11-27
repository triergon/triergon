"""Author the three studio-range rails and the 17-code cube."""
from hashlib import sha256
from itertools import product
import json
from pathlib import Path

from harness.palette_oracle import trace


def rails():
    for axis in range(3):
        for fixed in product((16, 128, 235), repeat=2):
            for code in range(256):
                yield fixed[:axis] + (code,) + fixed[axis:]


def write():
    directory = Path(__file__).resolve().parent
    cases = []
    for name, points in (("rails", rails()), ("cube", product(range(0, 256, 17), repeat=3))):
        path = directory / "inputs" / (name + ".rgba-codes")
        data = bytes(value for y, cr, cb in points for value in (y, cr, cb, y ^ cr ^ cb))
        path.write_bytes(data)
        for matrix, rounding in product(("bt601", "bt709"), ("nearest", "half-down")):
            rows = trace(path, matrix, rounding)
            cases.append({
                "id": f"{matrix}-{rounding}-{name}", "profile": f"{matrix}-{rounding}", "file": path.name,
                "sha256": sha256(data).hexdigest(), "rights": "authored-synthetic",
                "arguments": [matrix, rounding],
                "expected": {"trace_sha256": sha256(("\n".join(rows) + "\n").encode()).hexdigest()},
            })
    (directory / "corpus.json").write_text(json.dumps({
        "schema": 1, "directory": "harness/features/ycbcr-palette/inputs", "cases": cases,
    }, indent=2) + "\n")


if __name__ == "__main__":
    write()
