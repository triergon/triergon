# D4 measurement policy (declared 2026-11-03 ~02:40 UTC, before any D4 TIME/MEM qualification run)

Applies to `harness/d4/time_d4.py` (TIME), `.research/m1d4/tools/memory_driver.py` with the D4
plan from `.research/m1d4/tools/d4_mem_plan.py` (MEM) and `harness/d4/native_correctness.py`
(correctness). All on the own native Linux ARM64 runner (c8g.xlarge, Neoverse-V2), serialized,
never with a build, another benchmark or the RSS module loaded during TIME.

## Equivalent work
- Reference: `harness/d4/reference_decode.c` against the pinned FFmpeg 9.0.2 static libraries on
  the runner, the simulator's filter chains (letterbox scale/pad/format=rgba; aformat s16
  stereo). Decoder `thread_count = 1` and filter graph `nb_threads = 1`: the comparison is
  single-core work. The shipped `ffmpeg` CLI would slice-thread decoding and scaling across
  all CPUs; that is a resource choice, not different work, and is disclosed here. (Before this
  was fixed, development timings had the reference's scaling multi-threaded.)
- Candidate: `crates/sim` `d4-worker`, same access-unit split, same operations, same outputs.
- File loading and process start are outside the timed interval on both sides.
- Bench digest: an 8-lane word fold over every output byte, identical in both workers
  (`Digest` / `struct fold`). The earlier serial byte fold cost 9 ms per 1080p RGBA picture on
  both sides (two thirds of the reference's time) and would have compressed every ratio
  towards 1; it was replaced before any qualification run.
- TIME equal work: identical output unit count and byte count on every pair; digest equality is
  recorded (exact for bit-exact paths, expected to differ where the frozen rules allow 1 LSB).
- MEM equal work: the frozen engine requires identical work JSON, so memory mode reports
  `{packets, bytes}` on stdout and the digest on stderr (audio may differ within 1 LSB).
  Output exactness is graded only by the correctness gate under `NUMERIC-RULES.json`.

## Retry rules (first-clean otherwise)
- Each claim key (worker hashes, input, operation, iterations / mode) is measured once.
- TIME: a claim whose status is `contaminated` (CPU activity or module loaded during the
  pairs) may be recollected once on seed 281 with `--recollect-contaminated LABEL`; the
  original record stays nested as `contaminated_previous` and its raw file is kept.
- MEM: a case whose status is `observer-unqualified` or `invalid-memory-observation` (the
  frozen kernel bound or allocation guard rejected an observation, e.g. kernel error bit 1,
  an external task updating the process's RSS counters) may be recollected once on seed 281
  with `--recollect-invalid LABEL`; the original stays nested as `instrument_invalid_previous`.
  An interrupted reservation may be retried once (`--retry-interrupted`), as in M1.
- A clean failure (measured, gate not met) is never retimed for the same binaries.
- Development timings (`.research/m1d4/tools/d4_devtime.py`, dev ledgers under
  `~/d4/results-dev`) are never qualification evidence.

## Gates
- Correctness: every manifest vector (`CORRECTNESS-MANIFEST.json`, 454 vectors) at decoder level
  and output path; selection applied afterwards from the lane matrices; failures kept.
- TIME: 30 pairs, seed 280, upper95 <= 1.25 (target <= 0.80), per workload in `WORKLOADS.json`.
- MEM: 30 pairs, seed 280, heap and attributable RSS upper95 < 1, module unloaded after.

## Amendments (each recorded with what was known when it was made)

1. 2026-11-03 ~03:35 UTC, before any qualification run: `reference_decode` probes the stream
   once at load (`avformat_find_stream_info`) and copies the codec parameters into the
   decoder, as the CLI does. Without it the H.264 decoder dropped 2–9 out-of-order pictures
   on PAFF/MBAFF/B-pyramid vectors that the CLI outputs (dev4 correctness run).
2. 2026-11-03 ~06:15 UTC, **after** the D1 qualification run (its results are kept as
   first-clean in `D4-TIME-LEDGER.json` / `D4-MEM-LEDGER.json`): the probe is restricted
   to the video lanes. Reason, from the D1 MEM raw rows: the probe decodes frames before
   the measured window, so the reference process entered the window with its decoder code
   and tables already resident (audio `rss_start` 8.7–10.7 MB vs 4.4 MB without the probe)
   while the candidate faulted its own in-window; the attributable-RSS comparison was not
   like for like. Audio output is byte-identical with and without the probe (the probe only
   carries the H.264/HEVC reorder delay). The video lanes keep the probe, which still
   pre-warms the reference (conservative against the candidate). This changes the
   reference binary, so every D2 claim key differs from D1's; D1 is never retimed.
3. Also for D2 (candidate side, not the policy): `d4-worker` streams access units instead
   of indexing the whole file first, and the DTS decoder sizes its subband and synthesis
   buffers to the stream instead of the syntax maximum.
