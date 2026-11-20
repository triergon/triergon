# Comparing session queue storage

`cargo xtask session-queues` compares two builds of the public audio and video
sessions. Both workers use the same driver, inputs, decoder features, compiler
and target. The comparison measures a change to the queue in a real caller,
including decode, PCM mixing or RGBA conversion, and the output fold.

The [cases](cases.json) pin each source file, every access-unit length in order,
the input digest, and the decoded output counts and byte lengths. The worker
preloads the units through the Matroska or MPEG-TS facade before the measured
region. It gives each feed its submission index as a timestamp and drains
without waiting on a playback clock. The fold includes the published timestamps
and every output PCM sample or RGBA byte. Both revisions must agree on the full
fold on the target under test.

| Case | Units per pass | Distinct sizes | Adjacent equal sizes | Payload bytes at capacity | Largest unit |
|---|---:|---:|---:|---:|---:|
| AC-3, original track | 126 | 1 | 125 | 96,768 | 768 |
| ADTS AAC, original track | 95 | 51 | 0 | 24,919 | 312 |
| AVC, variable pictures | 24 | 24 | 0 | 35,925 | 4,463 |
| AVC, naturally stable run after startup | 24 | 2 | 22 | 1,673 | 1,305 |
| AVC, first random-access picture repeated | 24 | 1 | 23 | 31,320 | 1,305 |

The byte limit is the sum of one pass. The queue has one more slot than units,
and the single-unit limit is the largest payload. Each run fills the queue,
requires backpressure on the next borrowed feed, drains, retries that exact
unit and alternates feeding and draining for the remainder of a second pass.
A separate correctness run drains the warm refill to EOS and checks its additional output.
The memory observer takes separate runs through three endpoints.

- **cycle** drains to EOS and drops the session before the end marker.
- **starved** holds the fully drained, active session through the end marker.
- **full** refills that same warm session to its exact byte limit, checks
  backpressure again and holds the occupied queue through the end marker.

An allocation receipt covers the entire lane, including the decoder, output
buffers and queue metadata. The configured payload bound is in the work record.
Compare `window_allocation_calls`, `heap_peak_bytes` and
`heap_live_end - heap_live_start` on each heap pair. The full and starved runs
expose retention that disappears when EOS releases the lane. The separate RSS
observations and bounds use the repository's calibrated observer.

## Running a comparison

Build the example against each library revision in a private checkout. Overlay
the same example, session manifest and lockfile from the workload revision on
both. The following commands start in the workload checkout. Set `candidate`
to the session revision under test and `baseline` to its comparison base.

```sh
driver=$(git rev-parse HEAD)
scratch=$(mktemp -d)
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
runner="$scratch/xtask"
CARGO_BUILD_BUILD_DIR="$CARGO_TARGET_DIR/session-queues/runner-$driver" \
    cargo rustc --locked --release -p xtask -- --emit="link=$runner"
for role in baseline candidate; do
    if [ "$role" = baseline ]; then revision=$baseline; else revision=$candidate; fi
    tree="$scratch/$role-src"
    git worktree add --detach "$tree" "$revision"
    git archive "$driver" Cargo.lock crates/session/Cargo.toml \
        crates/session/examples/session_queue | tar -x -C "$tree"
    "$runner" session-queues build "$tree" "$scratch/$role"
done
"$runner" session-queues check "$scratch/baseline" "$scratch/candidate" "$scratch/work"
```

The build command uses the shared gate compiler, worker target and linker flags.
In an armhf user space it passes `--target thumbv7neon-unknown-linux-gnueabihf`
and copies the linked example from the intermediates for that source revision
and overlay. These intermediates live under the shared target directory.
Each worker has an
adjacent `.build.json` receipt recording its SHA-256, library revision, overlay
hashes, target, compiler and build command. Keep each executable with its receipt.
The comparison checks these identities and saves both receipts in `identity.json`.

On an isolated native Linux AArch64 or ARMv7 host, use the ordinary pinned
worker toolchain and CPU affinity. The memory run uses the prepared
`target/harness` observer and calibration executable in the workload checkout,
with the same observer and kernel proof requirements as the shared measurement
harness. Finish building both binaries and the driver before collecting samples.

```sh
taskset -c 1 "$runner" session-queues time "$scratch/baseline" "$scratch/candidate" "$scratch/time"
taskset -c 1 "$runner" session-queues memory "$scratch/baseline" "$scratch/candidate" "$scratch/memory"
```

Each command takes a fresh output directory. It records the binary and manifest
hashes, verifies every file hash and unit-size pattern, and checks the 20 paired
output folds before collecting measurements. Time uses 30 randomized pairs for
each complete cycle. Memory uses 10 pairs per case and endpoint with separate heap
and RSS processes. Per architecture this schedules 300 timed worker invocations
and 600 observed invocations before warmups, calibration and any harness retries.
Keep unsuccessful observations with the successful rows. The ratios describe the
two supplied workers. A measurement status records collection validity; the
per-case bounds and allocation counts determine whether the change helps.
