# HEVC comparison

`cargo xtask codecs compare-hevc` streams each named Annex B file through the HEVC
decoder example and FFmpeg. It uses the planar comparison from `codecs grade` and
prints one JSON row per file. To compare one file, build the example and point the
command at the decode profile's binaries.

```sh
cargo build --release -p triergon-codec-hevc --example decode_hevc
FF=.research/reference-decode/install/bin
cargo xtask codecs compare-hevc --decoder target/release/examples/decode_hevc \
  --ffmpeg "$FF/ffmpeg" --ffprobe "$FF/ffprobe" path/to/stream.bit
```

An exact match requires at least one picture, equal frame counts and samples, both
decoders exiting successfully and zero trailing bytes. The row includes both exit
codes and partial-byte counts. For the first differing picture, `first_diff` gives
the zero-based frame, plane, sample coordinates and picture order count when the
example reports it. `ctb64` gives the column and row on a 64×64 luma grid. It uses
luma units for chroma samples even when the stream signals a smaller CTB size.

The command exits successfully only when every file matches. A typed unsupported
feature with no candidate frames is recorded separately. The strict summary accepts
that refusal when the reference produces complete pictures in both YUV and RGBA.
If FFmpeg emits a prefix and then exits with an error, the manifest grade records
the candidate's diagnostic in `ours_errors` and the strict summary fails that vector.
If ffprobe gives no pixel format, both decoders run and the row records their byte
counts. Any emitted bytes then have unknown frame boundaries. FFmpeg also refuses
`EXT_A_ericsson_3.bit` (pre-v2 SPS extension syntax) and
`TSUNEQBD_A_MAIN10_Technicolor_2.bit` (10-bit luma and 9-bit chroma).

For the manifest-pinned comparison, including the RGBA output, run
`codecs grade --lanes hevc` followed by `codecs summarize --require-pass`.
[Grading commands](../STATUS.md#running-the-comparison) · [Decoder coverage](../../../crates/codec-hevc/README.md#conformance).
