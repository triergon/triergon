import unittest
from .runner import ROOT,differences
from .spec_oracle import timing_trace
import tempfile
from pathlib import Path
from tools import generate_header_corpus as fixture


class HeaderOracleTests(unittest.TestCase):
    def test_header_encoding_does_not_rewrite_source_timing(self):
        with tempfile.TemporaryDirectory() as directory:
            p=Path(directory)/"header.mkv";p.write_bytes(fixture.make(1,True,False))
            self.assertEqual([row['pts']['numerator'] for row in timing_trace(p)['packets']],[0,1_000_000])
