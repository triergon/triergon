"""Command-line entry to the independent Python oracles.

    python3 -m harness.oracles timing|content|chain|colour|catalogue FILE
    python3 -m harness.oracles inflate

Prints {"data": ...} or, for the malformed-input errors an oracle raises on
purpose, {"error": "..."}. Any other failure exits non-zero.
"""
import json
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
# Oracles that read no input file.
GENERATORS = {
    "inflate": inflate_cases,
}


def main():
    if len(sys.argv) == 3 and sys.argv[1] in ORACLES:
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
