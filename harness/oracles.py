"""Command-line entry to the independent Python oracles.

    python3 -m harness.oracles timing|content|chain|colour|catalogue FILE
    python3 -m harness.oracles KIND FILE [ARG...]
    python3 -m harness.oracles inflate

Prints {"data": ...} or, for the malformed-input errors an oracle raises on
purpose, {"error": "..."}. Any other failure exits non-zero.
"""
import json
from importlib import import_module
from pathlib import Path
import sys
import zlib

from .catalogue_oracle import source_catalogue
from .colour_oracle import source_colour
from .content_oracle import content_trace, encoding_chain
from .spec_oracle import timing_trace
from .zlib_oracle import inflate_cases

EXPECTED = (ValueError, IndexError, KeyError, StopIteration)
ORACLES = {
    "timing": (timing_trace, EXPECTED),
    "content": (content_trace, EXPECTED + (zlib.error,)),
    "chain": (encoding_chain, EXPECTED + (zlib.error,)),
    "colour": (source_colour, EXPECTED),
    "catalogue": (source_catalogue, EXPECTED),
}
# Trace oracles return canonical text rows without line endings. Load each on demand.
TRACE_ORACLES = {
    "views-smoke": ("harness.oracles", "views_smoke"),
}


def views_smoke(path):
    data = path.read_bytes()
    return [f"3 {len(data)} {zlib.adler32(data)}"]


# Oracles that read no input file.
GENERATORS = {
    "inflate": inflate_cases,
}


def main():
    if len(sys.argv) >= 3 and sys.argv[1] in TRACE_ORACLES:
        module, function = TRACE_ORACLES[sys.argv[1]]
        oracle = getattr(import_module(module), function)
        result = {"data": oracle(Path(sys.argv[2]), *sys.argv[3:])}
    elif len(sys.argv) == 3 and sys.argv[1] in ORACLES:
        oracle, expected = ORACLES[sys.argv[1]]
        try:
            result = {"data": oracle(Path(sys.argv[2]))}
        except expected as error:
            result = {"error": str(error)}
    elif len(sys.argv) == 2 and sys.argv[1] in GENERATORS:
        result = {"data": GENERATORS[sys.argv[1]]()}
    else:
        raise SystemExit(__doc__)
    json.dump(result, sys.stdout)


if __name__ == "__main__":
    main()
