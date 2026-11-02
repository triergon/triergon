# Research evidence and reproducibility

This folder contains the historical design/candidate evidence. The newer owned Rust workspace and shared harness are described in [implementation.md](../docs/implementation.md). No release-qualified library or approved third-party production graph is claimed.

- `source-pins.json`: immutable upstream source revisions; repository metadata and file license declarations can differ.
- `source-downloads.json`: hashes and paths of sampled upstream source files. The upstream code is not vendored. A zero lexical unsafe count is not a full source, macro, feature or transitive audit.
- `streem-inventory.json`: baseline commit, file hashes and responsibilities that actually need replacing.
- `fate-index.json`: historical 177 lexical identities from four n9.0 FATE definition files; original statuses remain unmapped/unreviewed. The newer harness maintains separate mapping/execution receipts: two upstream reference targets and three packet-demux comparisons, recorded in the qualification bundles.
- `module-gates.json`: frozen target interpretation and explicit unaccepted module states.
- `fixtures-manifest.json`, `environment.json`: synthetic fixture hashes, generation/reference tool versions and limitations.
- `results/`: raw ffprobe/candidate records and per-track payload/timestamp comparisons. These retain errors and mismatches.
- `spike/`: isolated evaluation adapters for published Symphonia 0.6.1 and matroska-demuxer 0.8.1. Its lockfile pins the evaluation graph; these are **not** production dependencies. Some upstream/transitive code does not meet the owner's strict-safe policy, which is one reason to evaluate it separately.

The collector completed 12 candidate/input invocations over five synthetic files. Nine completed full packet collection and three could not open the selected format/input combination. Packet collection success is not parity: timestamp mismatches, EOF errors, random-access/API gaps and post-EOF seek errors remain visible. No timings, memory measurements, TV tests, adversarial corpus or full-parity claim were produced.

## Run the exploratory comparison

```sh
python3 tools/generate-fixtures.py --output .research/fixtures
CARGO_INCREMENTAL=0 cargo +nightly build --manifest-path research/spike/Cargo.toml --locked
python3 tools/compare-candidates.py \
  --fixtures .research/fixtures \
  --binary research/spike/target/debug/safe-media-compat-spike \
  --output .research/rerun-results
```

Requires local FFmpeg with libx264/libx265/AAC encoders and ffprobe. Inputs contain synthetic test video, a generated sine wave and authored captions. Generation can vary with FFmpeg/encoder builds and container-generated metadata: every run records new hashes. Use the same materialized, hash-verified fixture files for exact A/B comparisons. The generator does not fetch household media.

The historical collector exits successfully when it has collected observations; **that is not a parity pass**. It must not be used as a release gate. The newer H0 harness adds canonical seek/metadata comparisons and fail-closed acceptance separately. The old Rust adapter caps packet count and uses small inputs/timeouts, but is not a hardened hostile-input runner.

## Recreate the partial FATE index

Obtain the FFmpeg source at the SHA recorded for `FFmpeg/FFmpeg` in `source-pins.json`, then:

```sh
python3 tools/inventory-fate.py --source /path/to/pinned/ffmpeg --output .research/fate-index.json
```

A full source checkout indexes more files than this initial four-file sample.
The configured reference build now records its `fate-list`; this does not mean
every listed target has run. The indexer reads text only and neither executes
upstream commands nor downloads samples. Selective local cache acquisition and
matched workers are implemented separately. Broader mapping/execution coverage
and sample redistribution rights remain open.
