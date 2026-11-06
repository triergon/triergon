# DVB ancillary-page EOD fixtures

Six authored display sets for `tests/ancillary_eod.rs`: end-of-display (EOD) on a
shared ancillary page, a display set split across two PES packets with the same
PTS, a non-empty EOD body (rejected), and three page-selection controls. One
control places EOD on the composition page after shared data; it tests decoder
tolerance, not conformance. `source.indices` and `source.rgba` are the expected
planes.

Reference: ETSI EN 300 743 V1.6.1 §7.2.6, with §§6.2 and 8 for framing and page
dependency.
