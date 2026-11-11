# Text fixtures and oracle

Corpus for `tests/text.rs`.

- `fixtures/`: 40 tx3g, UTF-8, Matroska WebVTT and ASS packet cases with
  source, configuration and addition bytes, expected UTF-8, times and fields, and
  the generated Rust case table; and the 32 SubRip and WebVTT documents of
  `manifest.json`'s `documents`, inputs of the `subtitle-text` comparison and
  the carriage view.
- `author.py`: builds the fixtures from authored components; needs `rustfmt`.
- `oracle.py`: independent parser using Python's strict codecs and `Fraction`
  clocks; it must agree with every expected value.
- `resource-cases.json`: twelve additional resource and recovery cases.
- `SOURCE-LOCK.json`: URLs and SHA-256 hashes of the specifications used.

Expected values come from the specifications, not from this crate or FFmpeg.
