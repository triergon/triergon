import unittest
from .runner import ROOT,differences
from .spec_oracle import timing_trace
import importlib.util
import tempfile
from pathlib import Path

spec=importlib.util.spec_from_file_location("header_generator",ROOT/"tools/generate-header-corpus.py")
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)


class HeaderOracleTests(unittest.TestCase):
    def test_header_encoding_does_not_rewrite_source_timing(self):
        with tempfile.TemporaryDirectory() as directory:
            p=Path(directory)/"header.mkv";p.write_bytes(fixture.make(1,True,False))
            self.assertEqual([row['pts']['numerator'] for row in timing_trace(p)['packets']],[0,1_000_000])
