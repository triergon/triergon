# Selected Matroska video on native Linux ARM64

Pinned FFmpeg and the Triergon worker matched all 44 frozen startup and seek
cases and 1,344 ordered packets on payload, PTS, DTS and duration. The
independent audit counted 43 delivery passes at 1.25× and 26 at the 0.80×
optimization target. All 44 passed lower live heap and total peak RSS. Their
worst upper ratios were 0.404× and 0.731× respectively.

The fresh HEVC no-Cues seek to 0 ms measured 1.2543× and remains a runtime
failure. The predecessor passed 22 of the same 44 cases. The operation
covers a selected immutable-input library session and uses 30 paired
measurements. Broader Matroska, network source adoption and TV verification
remain open.

[Per-case upper bounds and artifact identities](results.json) identify the
exact workers and the retained raw archive. The raw run files are stored
separately from this source checkout.
