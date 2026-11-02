# Deterministic mutation and differential checks (H06 development)

The shared runner `tools/fuzz-media.py` executes pinned JSON workers on original
inputs and deterministic byte mutations. It preserves every input, edit recipe,
raw stdout/stderr, worker result and difference. It does not grant feature parity,
timing, memory, FATE, module or target acceptance. Existing qualification gates and
the first-clean measurement policy remain unchanged.

## Coverage plan

| Boundary | Check | Required evidence |
|---|---|---|
| Reproduction | SHA-bound seed, ordered edits, final SHA | Replaying the saved recipe produces the exact saved input |
| Binary input | Bit flips, field overwrites, insertion, deletion, duplication and truncation | Empty and full-boundary seeds mutate deterministically without exceeding the input cap |
| Worker process | Elapsed-time and combined stdout/stderr limits | A timeout or output limit kills only the launched process group and remains incomplete evidence |
| Debug/release | Same source, input and operation | Exact JSON values agree; crashes, rejection differences and output differences remain findings |
| Native reference | Same bytes and declared operation | Exact payload, unknowns, booleans and packet order are compared without normalization |
| Harness protocol | Strict JSON | Duplicate fields, non-finite values, invalid output and missing comparisons cannot become agreements |
| Campaign controls | Declared original-input outcomes | Every seed/operation baseline runs before its mutations; changed outcomes are findings |
| Provenance | Worker, source, manifest and controller hashes before/after | A changed source or executable prevents a completed stable-source report |

`harness.test_fuzz` covers recipe tampering, dependent edit order, invalid ranges,
binary/empty/full inputs, strict comparisons, process limits and a complete
three-worker campaign that deliberately disagrees. The campaign test checks that
raw failures and replayable inputs survive and that qualification remains false.
`harness.test_fuzz_integrity` checks exact numeric values, manifest replacement,
input tampering, deep JSON and descendant cleanup independently with mocked
worker boundaries. Fractional JSON values retain exact decimal values within the
finite binary64 protocol range; differing decimals appear as `json_decimal` text
tags in findings. Integers stay exact and booleans remain distinct. The protocol
bounds nesting at 256 containers and traversal at one million values. Exceeding
either produces a retained protocol/comparison finding, never an agreement.

## Manifest and execution

The manifest has schema 1, a nonempty `workers` map, a source-file hash map,
operation definitions and seeds. Every worker has `path` and `sha256`. Every seed
has `id`, `path`, `sha256`, `rights` and `baseline_statuses` for each operation and
worker role. Paths can be absolute or relative to the repository. Output must be
a new directory under `.research/`; no old evidence is overwritten.
The manifest is read once, hashed and parsed from that same buffer, and those
exact bytes are preserved. Replacing the manifest later prevents a stable report.

An operation defines a bounded label and argument lists by role. Arguments contain
exactly one literal `{input}` slot; the runner substitutes the saved input path
and executes the pinned worker directly, without a shell. For example:

```json
{"id":"packets","roles":{
  "candidate":["trace","{input}"],
  "debug":["trace","{input}"],
  "reference":["trace","{input}"]
}}
```

Run the prepared manifest with:

```sh
python3 tools/fuzz-media.py --manifest .research/campaign-manifest.json \
  --output .research/campaign-new --rounds 4 --seed 280
```

Each seed receives its unchanged baseline followed by the requested number of
mutations. Recipes bind offsets to each intermediate byte sequence and bind both
the seed and final result with SHA-256. All worker roles see the same saved input.
Each case preserves its initial bytes separately and checks its input hash and
length before and after every role and before saving the final observation.
Changed input becomes structured incomplete evidence; subsequent roles skip that
changed pathname. The campaign records failed input identity and exits nonzero.
The finding index streams to JSONL, while per-case files retain the raw outputs;
the controller does not retain a whole campaign's parsed packet values in memory.

`report.json` distinguishes observed agreements, ungraded paired rejections,
acceptance disagreements, protocol failures, crashes and incomplete observations.
Exit 1 means findings were retained. Exit 0 means this campaign completed without
those findings; it never means a selected feature or module is qualified. A pair
of parser errors is not a feature-parity pass. Native disagreements require
source/specification review, including the original input-bound comparison rules.
The runner does not silently apply those rules to modified inputs.

The default watchdog bounds input size, captured output and elapsed time. It is
not an OS memory sandbox or a memory/performance measurement. Parser limits and
separate calibrated memory tests remain required. The process watchdog currently
requires a POSIX host. No Mac result establishes TV or 32-bit behavior.
Cleanup terminates the owned process group even after its leader has exited.

Stateful open/read/seek/reset/cancel scheduling, retained-output pressure, transport
faults, structural generators and sustained cross-target campaigns remain required
H06 work. This wire-mutation component does not close that milestone.
