"""Command-line entry to the independent Python oracles.

    python3 -m harness.oracles timing|content|colour|catalogue FILE

Prints {"data": ...} or, for the malformed-input errors an oracle raises on
purpose, {"error": "..."}. Any other failure exits non-zero.
"""
import json
from pathlib import Path
import sys
import zlib

from .catalogue_oracle import source_catalogue
from .colour_oracle import source_colour
from .content_oracle import content_trace
from .spec_oracle import timing_trace

EXPECTED = (ValueError, IndexError, KeyError, StopIteration)
ORACLES = {
    "timing": (timing_trace, EXPECTED),
    "content": (content_trace, EXPECTED + (zlib.error,)),
    "colour": (source_colour, EXPECTED),
    "catalogue": (source_catalogue, EXPECTED),
}


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ORACLES:
        raise SystemExit(__doc__)
    oracle, expected = ORACLES[sys.argv[1]]
    try:
        result = {"data": oracle(Path(sys.argv[2]))}
    except expected as error:
        result = {"error": str(error)}
    json.dump(result, sys.stdout)


if __name__ == "__main__":
    main()
