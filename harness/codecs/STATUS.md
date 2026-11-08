# Decoder comparison material

Inputs, rules and tools used to compare the codec crates (`crates/codec-aac`,
`codec-ac3`, `codec-dca`, `codec-flac`, `codec-h264`, `codec-hevc`, `codec-mp3`,
`codec-opus`, `codec-vorbis`), the session crate (`crates/session`) and the audio
container readers (`crates/ogg`, `crates/elementary`) with FFmpeg 9.0.2. What each
decoder supports and refuses is in its crate README. The comparison harness is the
decode build of `harness/reference.c` and the decode profile's `ffmpeg` and `ffprobe`,
driven by `cargo xtask codecs` (`manifest`, `grade`, `summarize`, `readers`, `time`,
`memory`, `equivalence`; see `cargo xtask codecs --help`). Gate definitions:
[docs/quality.md](../../docs/quality.md).

| Path | Content |
|---|---|
| `numeric-rules.json` | Comparison rules `cargo xtask codecs grade` applies: lossy audio per channel RMS ≤ 8.8e-6 and max ≤ 6.1035e-5 with exact rate, layout and counts; FLAC bit-exact; video bit-exact; RGBA output bit-exact; stereo s16 output ≤ 1 LSB; and the reviewed rules for the Layer I and II float decoders and the integer stereo mix of 16-bit FLAC |
| `correctness-manifest.json` | The 775 correctness vectors: 454 of the video and elementary-stream lanes and 321 of the file-format lanes (FLAC 187, MP3 39, MP2 20, MP1 5, Vorbis 21, Opus 49) |
| `reader-manifest.json`, `reader-rules.json`, `reader-differences.json` | The 305 reader comparison inputs; the views and fields compared per format, the reviewed comparison rules and the scoped dispositions of FFmpeg behaviours `cargo xtask codecs readers` applies; and every reviewed difference with its disposition |
| `workloads.json`, `file-workloads.json`, [workloads.md](workloads.md) | The 17 elementary-stream and 10 file-format timing and memory workloads, and how they are generated |
| [measurement-policy.md](measurement-policy.md) | What counts as equivalent work in timing and memory runs |
| [adapter-contract.md](adapter-contract.md) | Session-layer contract for a host adapter |
| `downmix-*.json`, [downmix-integer.md](downmix-integer.md) | Observed stereo downmix (`-ac 2`) rules, for float and for integer decoder output |
| `rgba-*.json` | Observed letterbox scaling and RGBA conversion rules |
| `codec-comparison` evidence set | Correctness rows and summaries, timing and memory records, identity hashes; `cargo xtask evidence fetch codec-comparison` unpacks it into `.research/evidence/codec-comparison/` |
| `audio-review-linux-arm64`, `audio-quick-check-linux-arm64` evidence sets | The file-format lanes' correctness rows, instruction counts, paired timing and paired memory the status rows cite; the reader lane with both Ogg views, the generated Ogg inputs it compares (`inputs/ogg-generated`), and the quick check of the head against main |
| `aac/`, `ac3/`, `dca/`, `h264/`, `hevc/` | Per-codec tools and derivation notes |
| `flac/`, `mp3/`, `ogg/` | Generators of the FLAC, MPEG audio and Ogg vectors |
| `codec-reference-binary`, `codec-reference-reproduction` evidence sets | The reference worker the cited results were measured with, and the proof that `prepare-ffmpeg --profile decode` rebuilds it: byte-identical output on every vector and workload, identical classifications |

## Running the comparison

```sh
cargo xtask prepare-ffmpeg --profile decode   # FFmpeg 9.0.2, default configuration
cargo xtask build-workers                     # ffmpeg-decode-worker, codec-worker, …
FF=.research/reference-decode/install/bin
cargo xtask codecs grade --manifest harness/codecs/correctness-manifest.json --root . \
  --bin DIR --ffmpeg $FF/ffmpeg --ffprobe $FF/ffprobe \
  --codec-worker target/release/codec-worker \
  --reference target/harness/ffmpeg-decode-worker --out run.jsonl
cargo xtask codecs summarize run.jsonl summary.json
cargo xtask codecs readers --manifest harness/codecs/reader-manifest.json --root . \
  --ffprobe $FF/ffprobe --codec-worker target/release/codec-worker --out readers.jsonl
```

`--bin` names a directory with the decoder examples as `codec-h264-decode`,
`codec-hevc-decode`, `codec-ac3-decode`, `codec-aac-decode` and `codec-dca-decode`
(`target/release/examples/decode_{h264,hevc,ac3,aac,dts}`), and `audio-decode`
(`crates/session/examples/decode_audio`, built with `--features codecs`) for the `flac`,
`mp3`, `mp2`, `mp1`, `vorbis` and `opus` lanes, which read their file through the
container readers and apply its trims as the CLI does. Every run checks each input
against its manifest SHA-256 and writes one row per input, whatever its outcome.

The vectors are the FATE suite's `aac`, `ac3`, `eac3`, `dts`, `h264-conformance` and
`hevc-conformance` directories in `.research/fate-samples`, plus 71 elementary streams
derived from them with the decode profile's `ffmpeg` (manifest paths under `target/`):
ADTS from the MP4 and 3GP vectors (`-map 0:a:0 -c:a copy -f adts`), DTS cores
(`-map 0:a:0 -c:a copy -bsf:a dca_core -f dts`) and AC-3 streams copied out of their
containers (`-map 0:a:N -c:a copy -f ac3`). The file-format lanes add FATE's `vorbis`,
`opus` and `mp3-conformance` directories, the IETF CELLAR FLAC test files (`subset`,
`uncommon`; CC0), the FLAC streams `harness/codecs/flac/flac_vectors.py FFMPEG FLAC
.research/flac-generated` encodes, the MPEG audio streams
`harness/codecs/mp3/make_vectors.sh FFMPEG .research/mp3/vectors` writes (Layer III with
libmp3lame, so FFMPEG is an FFmpeg 9.0.2 build with it) and 34 generated Opus streams
in `.research/opus-generated`. `cargo xtask codecs manifest` rehashes them:

```sh
cargo xtask codecs manifest --out harness/codecs/correctness-manifest.json \
  h264=.research/fate-samples/h264-conformance hevc=.research/fate-samples/hevc-conformance \
  ac3=target/agent-ac3/es eac3=target/agent-ac3/es aac=target/agent-aac/vec \
  dts=target/agent-dca/vec \
  flac=.research/flac-test-files/subset,.research/flac-test-files/uncommon,.research/flac-generated \
  mp3=.research/fate-samples/mp3-conformance,.research/mp3/vectors mp2=.research/mp3/vectors \
  mp1=.research/mp3/vectors vorbis=.research/fate-samples/vorbis \
  opus=.research/fate-samples/opus,.research/opus-generated
cargo xtask codecs manifest --out harness/codecs/reader-manifest.json \
  reader-ogg=.research/fate-samples/vorbis,.research/ogg-generated \
  reader-flac=.research/elementary,.research/flac-test-files,.research/flac-generated \
  reader-mp3=.research/elementary,.research/fate-samples/mp3-conformance \
  reader-aac=.research/elementary reader-ac3=.research/elementary \
  reader-eac3=.research/elementary
```

## File-format lanes

The `flac`, `mp3`, `mp2`, `mp1`, `vorbis` and `opus` lanes compare with FFmpeg's native
decoders by name. FLAC is lossless: its float output must equal FFmpeg's exactly
(`lossless_audio_decode`), with the same rate, layout and sample counts; the other
lanes take the lossy audio rule. Layer I and II are compared with FFmpeg's float
decoders `mp1float` and `mp2float`, whose fixed-point defaults round to 16 bits
(`mpeg-audio-float-decoders`). The stereo s16 output of 16-bit FLAC is FFmpeg's integer
mix, which `codec-worker` reproduces (`flac-integer-device-mix`).

## Reader lane

`cargo xtask codecs readers` compares the packets the container readers emit with the
decode profile's `ffprobe -show_packets`, the demuxer forced on both sides. Its inputs
are FATE's `vorbis` and `mp3-conformance` directories, the Ogg files
`harness/codecs/ogg/make_vectors.sh` generated (archived in `audio-quick-check-linux-arm64`
under `inputs/ogg-generated`; copy them to `.research/ogg-generated`), the FLAC test
files and generated FLAC streams, and the streams in `.research/elementary` (encoded by
FFmpeg, and crafted damaged ones in `crafted/`). `codec-worker packets FORMAT FILE`
prints every packet `triergon_ogg::BlockingReader` or the `triergon_elementary` reader
emits: track, pts, dts, duration, size, position and trims as the reader reports them,
for Ogg also its FFmpeg-compatible view (`triergon_ogg::compat::clock::packet_timing`)
and the reader's resync and lost-page events.

`reader-rules.json` projects the reader's times into FFmpeg's stream time base, aligns
packets per stream (by position for elementary streams, by size for Ogg, where the
reader reports the page that completes a packet and FFmpeg the one it starts on) and
compares every field in each view: `native`, what the reader reports, and for Ogg
`compat`, its FFmpeg-compatible policy. A native difference the compat view does not
have is `reproduced-by-compat`. Every other difference must be one of the reviewed
differences of its case and view in `reader-differences.json`, one per line (stream,
reader packet, reference packet, field, reader value, FFmpeg value, disposition) under
the SHA-256 of the list, and fall within the scope (view, fields, packets, value
relation) of exactly one disposition the case lists. Each view of each input is `pass`,
`refused-by-both`, `reproduced-by-compat`, `disposition` or `fail`. The run writes its
unexplained differences, attributed where a scope matches, to `OUT.differences.json`
for review.

## Timing and memory

`cargo xtask codecs time` pairs the workers on the workloads (30 pairs, seed 280).
`cargo xtask codecs memory --candidate target/release/codec-worker --reference
target/harness/ffmpeg-decode-worker --workloads harness/codecs/file-workloads.json
--root . --out DIR` measures both workers' `OP-memory` runs under the shared observer
of `target/harness` (30 pairs, seed 282). Each worker's work is checked first and must
be equal; the gate is the paired 95% upper bounds of peak heap
(`heap_ratio_upper_95`) and total resident peak RSS (`total_rss_ratio_upper_95`), both
below 1.0, on native AArch64 Linux with the gate build. `DIR` keeps every raw
observation per workload and `summary.json`.

## Dispositions cited by the correctness summaries

- AC-3/E-AC-3 noise: bap-0 dither, AHT zero-bin fill and SPX blending noise are random.
  A decoder matches FFmpeg's noisy output only by reproducing its noise stream, which
  `codec-ac3` does under `compat: Compat::Reference`. Both correctness summaries in the
  `codec-comparison` evidence set were collected with a build that used its own
  generator; they record 10 AC-3/E-AC-3 vectors as failures for this reason. The set's
  `CORRECTNESS-MANIFEST-AC3-NODITHER.json` is the dither-stripped comparison set.
- `aac-sce-in-stereo.adts`: FFmpeg leaves FR unwritten in single-SCE frames; see the
  known differences in [crates/codec-aac](../../crates/codec-aac/README.md).
