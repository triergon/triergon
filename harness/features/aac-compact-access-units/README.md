# Compact AAC access-unit comparison

The input is the original FATE `al04sf_48.mp4`, kept outside git. Its SHA-256 is
`7dc63a8bbf3818af6e771ff359220ab9e0fe63fb0fad8550f55ccadb2df1a40b`.
The comparison extracts the original configuration and all 386 packet byte ranges in
decode order. It requires the pinned FFmpeg 9.0.2 decode
profile and workers built for the target, with their build identity.

On each native gate target, prepare the reference and build the workers as described
in the repository's quality guide, then run from the repository root with a fresh
output directory:

```sh
CARGO_INCREMENTAL=0 cargo xtask build-workers --only codec-aac-worker
CARGO_INCREMENTAL=0 cargo xtask codecs aac-compact-access-units --source /path/to/al04sf_48.mp4 --out /path/to/compact-inputs
```

Run the existing `cargo xtask codecs time` and `cargo xtask codecs memory`
collectors with the `workloads.json` and root written by the comparison.
Use `--no-early-stop` for both collectors and `--pairs 30` for memory. Pass the candidate built
for the host target and `target/harness/ffmpeg-aac-compact-worker` as the reference.
Both collectors verify `parity.json` and bind the complete pinned workload to
the same worker identities.

For a direct non-timing PCM trace after preparation, invoke either worker with
`audio-aac-access-units-compact-raw /path/to/compact-inputs/al04sf_48.aac-access-units /path/to/output.pcm`.

The independent known-spectrum check exercises 48 kHz mono compact AAC without a
source file. With the same prepared worker identities, run it in a fresh directory:

```sh
CARGO_INCREMENTAL=0 cargo xtask codecs aac-known-spectrum --out /path/to/known-spectrum
```

It generates 98 long and 224 short cases: the first and last spectral bins in
all 49 long and 14 short bands, at each of the eight short-window positions.
Each case compares all 2,880 samples from both workers to a direct cosine IMDCT
with a sine window and to each other, with a strict absolute bound of `2e-8`.
The inputs use one +1 codebook-1 coefficient, separately grouped short windows,
and silent surrounding blocks. The check does not cover interior bins, other
codebooks or values, KBD windows, grouped short windows, or other spectral tools.
It is functional conformance only and adds no timing or memory workload. The
original source comparison above remains a separate check.

Both workers receive the same raw blocks. The reference receives ASC `11 8c`;
the candidate receives ASC `11 84 04 c4 00 20 01 e0 00`, which carries the
mono program configuration required by its compact decoder. Both describe a
48 kHz mono SCE with tag zero and 960-sample frames.
