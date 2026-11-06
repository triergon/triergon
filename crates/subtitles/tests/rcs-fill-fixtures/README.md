# DVB region fill and background fixtures

Four authored two-display sequences for `tests/rcs_fill.rs`. The first display
fills every pixel of a 4x2 region; the second changes the background code with
fill clear, or is an unchanged-background or active-fill control. Object updates
carry a current ODS; partial CLUT updates keep the earlier pixels.

The two sequences that change an inactive background violate EN 300 743 §5.1.5;
they test that the decoder tolerates such input, not conformance.

Reference: ETSI EN 300 743 V1.6.1 §§5.4.3 and 7.2.3–7.2.5.
