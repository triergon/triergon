import unittest
from .runner import ROOT,differences
from .spec_oracle import timing_trace
import tempfile
from pathlib import Path
from .common import xtask_fixture


class HeaderOracleTests(unittest.TestCase):
    def test_header_encoding_does_not_rewrite_source_timing(self):
        with tempfile.TemporaryDirectory() as directory:
            p=Path(directory)/"header.mkv";p.write_bytes(xtask_fixture('header','--scope',1,'--lace'))
            self.assertEqual([row['pts']['numerator'] for row in timing_trace(p)['packets']],[0,1_000_000])
