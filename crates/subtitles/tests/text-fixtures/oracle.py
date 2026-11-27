#!/usr/bin/env python3
"""Run the independent authored source-fact check."""
import runpy
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[4]))
runpy.run_module("harness.text_extraction_oracle", run_name="__main__")
