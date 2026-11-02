# Native Mac ARM64 K0/K1 qualification

These receipts qualify the exact selected slices and corpus on native macOS ARM64.
They do not qualify the complete M01/M02 modules, Streem integration or webOS hardware.
The owner deferred Linux x86_64 and webOS/ARM32 results for current development;
the parity, runtime and lower-memory thresholds remain unchanged.

| Contract | Cases | Maximum runtime ratio, upper 95% | Maximum attributable RSS bound ratio, upper 95% | Maximum live-heap ratio, upper 95% |
|---|---:|---:|---:|---:|
| K0 compressed-packet demux | 12 | 0.769421 | 0.230770 | 0.176096 |
| K1 header removal | 4 | 0.228146 | 0.222223 | 0.053139 |

All cases pass exact FFmpeg packet compatibility, exact independent source-time
comparison and their declared short-read/seek checks. Timing and each memory lane
have 30 raw pairs. The runtime threshold remains 0.80; both memory bounds must be
strictly below 1.0. See [the summary](summary.json), [K0 report](k0/report.json) and
[K1 report](header/report.json). Ratios in this table are rounded upward.

The reports retain raw timings, randomized pair order, identical-work checksums,
compiler/encoder CPU-counter isolation evidence, memory calibration certificates,
heap/RSS observations, input hashes and source/build/binary identities. Authored
canonical traces accompany the cases. The [source snapshot](source/) contains the
owned implementation and all hashed build/harness inputs; it has no external Rust
dependencies or third-party implementation source. Reference FFmpeg is a separate
pinned n9.0 build. The identity's installed `ffmpeg_version` field describes the
synthetic-input generator, not the benchmark's native reference library.

The first K0 window was contaminated by another compiler; its report is retained as
[a non-pass](contaminated-k0-attempt-0.json). The next window reuses already complete
clean cases and measures the contaminated case again. No valid slower run was
discarded. The camera capture was briefly paused with owner authorization, protected
by cleanup and an independent resume watchdog, and resumed after each window. Both
[window 0](mac-final-window-0.json) and [window 1](mac-final-window-1.json) record this.
The controller is preserved in `qualification-controller.py`.

[Xiph FATE evidence](fate-xiph.json) includes an actual upstream reference FATE
execution plus the 84-packet demux comparison. The
[zero-length-block evidence](fate-zero-length.json) checks both packets and source
timing but explicitly does not execute the complete upstream remux test. Its late
Tracks require seekable input. External FATE samples and raw external payloads
remain in the ignored cache; redistribution rights are unreviewed.

`slice_accepted` is true for these contracts. The broader `accepted` field remains
false because remaining module requirements and complete applicable FATE coverage
are pending. These pending requirements have not been removed to manufacture a pass.
