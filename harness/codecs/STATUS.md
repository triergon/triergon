# Decoder comparison material

Inputs, rules and tools used to compare the codec crates (`crates/codec-aac`,
`codec-ac3`, `codec-dca`, `codec-flac`, `codec-h264`, `codec-hevc`, `codec-mp3`,
`codec-opus`, `codec-vorbis`), the session crate (`crates/session`) and the audio
container readers (`crates/ogg`, `crates/elementary`) with FFmpeg 9.0.2. What each
decoder supports and refuses is in its crate README. The reference is the decode build
of `harness/reference.c` with the decode profile's `ffmpeg` and `ffprobe`, driven by
`cargo xtask codecs` (`--help` lists its commands). Gate definitions:
[docs/quality.md](../../docs/quality.md); results: [docs/status.md](../../docs/status.md).

| Path | Content |
|---|---|
| `numeric-rules.json` | Comparison rules `cargo xtask codecs grade` applies: lossy audio per channel RMS ≤ 8.8e-6 and max ≤ 6.1035e-5 with exact rate, layout and counts; FLAC bit-exact; video bit-exact; RGBA output bit-exact; stereo s16 output ≤ 1 LSB; and the reviewed rules for the Layer I and II float decoders and the integer stereo mix of 16-bit FLAC |
| `correctness-manifest.json` | The correctness vectors of every lane, pinned by SHA-256 |
| `derived-streams.json` | The FATE source, its SHA-256 and the `ffmpeg` arguments of each derived elementary stream of the aac, ac3, eac3 and dts lanes (`cargo xtask codecs derive`) |
| `reader-manifest.json`, `reader-seeks.json`, `reader-rules.json`, `reader-differences.json` | The reader comparison inputs and Ogg seek cases; the views, fields, rules and scoped dispositions `cargo xtask codecs readers` applies; every reviewed difference with its disposition |
| `workloads.json`, `file-workloads.json`, [workloads.md](workloads.md) | The elementary-stream and file-format timing and memory workloads, and how they are generated |
| [measurement-policy.md](measurement-policy.md) | What counts as equivalent work in timing and memory runs |
| [adapter-contract.md](adapter-contract.md) | Session-layer contract for a host adapter |
| `downmix-*.json`, [downmix-integer.md](downmix-integer.md) | Observed stereo downmix (`-ac 2`) rules, for float and for integer decoder output |
| `rgba-*.json` | Observed letterbox scaling and RGBA conversion rules |
| `aac/`, `ac3/`, `dca/`, `h264/`, `hevc/` | Per-codec tools and derivation notes |
| `flac/`, `mp3/`, `ogg/` | Generators of the FLAC, MPEG audio, Ogg and Speex vectors |
| `opus/` | Authored family 2 probe and its float PCM, frame length and layout comparison with FFmpeg native Opus |
| `codec-comparison`, `audio-review-linux-arm64`, `audio-quick-check-linux-arm64` evidence sets | Recorded comparison runs: correctness rows, timing and memory records, identity hashes; `cargo xtask evidence fetch SET` unpacks one into `.research/evidence/SET/` |
| `codec-reference-binary`, `codec-reference-reproduction` evidence sets | The reference worker, and the proof that `prepare-ffmpeg --profile decode` rebuilds it (byte-identical output on every vector and workload) |

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
  --seeks harness/codecs/reader-seeks.json \
  --ffprobe $FF/ffprobe --codec-worker target/release/codec-worker --out readers.jsonl
```

`--bin` names a directory with the decoder examples as `codec-h264-decode`,
`codec-hevc-decode`, `codec-ac3-decode`, `codec-aac-decode` and `codec-dca-decode`
(`target/release/examples/decode_{h264,hevc,ac3,aac,dts}`), and `audio-decode`
(`crates/session/examples/decode_audio`, built with `--features codecs`) for the `flac`,
`mp3`, `mp2`, `mp1`, `vorbis` and `opus` lanes, which read their file through the
container readers and apply its trims as the CLI does. Every run checks each input
against its manifest SHA-256 and writes one row per input, whatever its outcome.
A manifest may list `open_cases`, names of its vectors that fail today. `summarize`
counts such a row as `open` when it fails and needs a disposition for it; it fails
when a listed vector no longer fails, so the list only shrinks, as a contract's open
cases do ([docs/quality.md](../../docs/quality.md)). `grade --lanes LANE,...` grades
those lanes alone, and `summarize --require-pass` also fails on any other failing
vector. The gate item `codecs-audio` runs both over the aac, ac3, eac3 and dts lanes on
both gate targets.

The [AAC MOV manifest](aac/mov-manifest.json) uses the same `codecs grade` command
with the `audio-decode` example. It compares all PCM from the neutral reader's
original access units. The reference reads the original sample table with
`-ignore_editlist 1` and emits the complete decoder output with
`-flags2 +skip_manual`. The comparison checks rate, speaker layout, sample count
and every float sample under the AAC accuracy rules.
For fixed configurations 5 and 6, FFmpeg labels the single surround pair as
back and the decoder labels it as side. The comparison records both labels
and checks both corresponding PCM planes in their original order.

The video lanes' vectors are the FATE suite's `h264-conformance` and `hevc-conformance`
directories in `.research/fate-samples`. The aac, ac3, eac3 and dts lanes read 71
elementary streams in `.research/codec-derived`, made from the FATE suite's `aac`,
`ac3`, `eac3` and `dts` directories as [derived-streams.json](derived-streams.json)
records: ADTS from the MP4 and 3GP vectors (`-map 0:a:0 -c:a copy -f adts`), DTS cores
(`-map 0:a:0 -c:a copy -bsf:a dca_core -f dts`), AC-3 streams copied out of a WAV and a
transport stream (`-map 0:a:N -c:a copy -f ac3`), and 17 FATE files copied unchanged
(raw ADTS, AC-3 and E-AC-3; the `_small` streams are FATE's own cuts).
`cargo xtask codecs derive --ffmpeg FF`, with the decode profile's `ffmpeg`, fetches each
source by its SHA-256, makes every stream and fails on one that differs from its
manifest pin. Evidence set `codec-derived-streams` holds them at their repository
paths, and the gate images unpack it. The aac lane adds three crafted HE-AAC v2 streams
of Parametric Stereo paths no FATE vector takes, in `aac/ps/vectors`
([aac/ps/README.md](aac/ps/README.md)). The file-format lanes add FATE's `vorbis`,
`opus` and `mp3-conformance` directories, the IETF CELLAR FLAC test files (`subset`,
`uncommon`; CC0), the FLAC streams `harness/codecs/flac/flac_vectors.py FFMPEG FLAC
.research/flac-generated` encodes, the MPEG audio streams
`harness/codecs/mp3/make_vectors.sh FFMPEG .research/mp3/vectors` writes (Layer III with
libmp3lame, so FFMPEG is an FFmpeg 9.0.2 build with it) and 34 generated Opus streams
in `.research/opus-generated`. `cargo xtask codecs manifest` rehashes them:

```sh
cargo xtask codecs manifest --out harness/codecs/correctness-manifest.json \
  h264=.research/fate-samples/h264-conformance hevc=.research/fate-samples/hevc-conformance \
  ac3=.research/codec-derived/ac3 eac3=.research/codec-derived/ac3 \
  aac=.research/codec-derived/aac,harness/codecs/aac/ps/vectors dts=.research/codec-derived/dts \
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
are FATE's `vorbis` and `mp3-conformance` directories, FATE's Theora samples
(`ogg/bear.ogv`, `ogg/empty_theora_packets.ogv`, `vp3/offset_test.ogv`) and chained
`ogg-flac`, `ogg-opus` and `ogg-vorbis/chained-meta.ogg`, the Ogg files
`harness/codecs/ogg/make_vectors.sh` generated (`.research/ogg-generated`) and the
Speex files `harness/codecs/ogg/make_speex_vectors.sh` generated with speexenc 1.2.1
(`.research/speex-generated`), the FLAC test files and generated FLAC streams, and the
streams in `.research/elementary` (encoded by FFmpeg, and crafted damaged ones in
`crafted/`). `codecs manifest` lists the Ogg lane's first two directories; its other
rows are kept in the manifest as they are, the chained samples named with their
directory. Evidence set `ogg-reader-inputs` holds every input of the Ogg lane at its
repository path. `codec-worker packets FORMAT FILE` prints every packet
`triergon_ogg::BlockingReader` or the `triergon_elementary` reader emits: track, pts,
dts, duration, size, position and trims as the reader reports them, for Ogg also its
FFmpeg-compatible view (`triergon_ogg::compat::clock::packet_timing`), the granule
position of a packet that ends its page, and the reader's resync and lost-page events.
Ogg compares FFmpeg's audio and video streams; a reader track that is a stream of
another type in FFmpeg (Ogg Skeleton) is counted, not compared.

Each case of `reader-seeks.json` (lane `reader-ogg-seek`) runs `codec-worker seek ogg
FILE TRACK SECONDS exact|preroll`, which seeks the reader and prints its receipt and
every packet after it, against `ffprobe -read_intervals SECONDS%` with the target less
the reader's preroll. FFmpeg's packets before the reader's landing are counted, not
compared; the rest are aligned by position and size and compared like a whole file,
and their differences are reviewed per case by name. The gate runs both Ogg lanes
(item `ogg-readers`); the command fails when a case fails.

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

`cargo xtask codecs isobmff-reader` applies the same alignment and the same reviewed,
frozen difference lists to the neutral ISO BMFF reader, against the formats profile's
`ffprobe` with the MOV demuxer, and is a gate item (`isobmff-reader`). Its contract,
corpus of read and seek cases, rules and differences are in
[harness/features/isobmff-reader](../features/isobmff-reader); it fails when a case
fails:

```sh
cargo xtask codecs isobmff-reader --ffprobe .research/reference-formats/install/bin/ffprobe \
  --codec-worker target/release/codec-worker --out isobmff-reader.jsonl
```

## Timing and memory

`cargo xtask codecs time` pairs the workers on the workloads (30 pairs, seed 280,
retried on 281 and 282; [early stop](../../docs/quality.md#early-stop) applies).
`cargo xtask codecs memory --candidate target/release/codec-worker --reference
target/harness/ffmpeg-decode-worker --workloads harness/codecs/file-workloads.json
--root . --out DIR` measures both workers' `OP-memory` runs under the shared observer
of `target/harness` (30 pairs, seed 283). Each worker's work is checked first and must
be equal; the paired 95% upper bounds of peak heap (`heap_ratio_upper_95`) and total
resident peak RSS (`total_rss_ratio_upper_95`) are graded by the
[memory gate](../../docs/quality.md#gates), with the gate build on native Linux AArch64
or in the 32-bit ARMv7 user space.
`DIR` keeps every raw observation per workload and `summary.json`.

## Dispositions cited by the correctness summaries

- AC-3/E-AC-3 noise: bap-0 dither, AHT zero-bin fill and SPX blending noise are random.
  A decoder matches FFmpeg's noisy output only by reproducing its noise stream, which
  `codec-ac3` does under `compat: Compat::Reference`. The `codec-comparison` set's
  `CORRECTNESS-MANIFEST-AC3-NODITHER.json` is the dither-stripped comparison set.
- `aac-sce-in-stereo.adts`: FFmpeg leaves FR unwritten in single-SCE frames. The float
  comparison of this exact input (by SHA-256) uses `ffmpeg-aac-initialized-pcm`, which
  initializes both planes before decoding, and requires successful decoding and all
  222,208 samples per channel in both the float and stereo s16 comparisons. The stereo
  s16 comparison and the measurement workers keep the ordinary reference.
- `xll_51_24_48_none.core.dts` carries no DTS core substream, only the lossless
  extension, which FFmpeg decodes (3,072 samples of 5.1) and this decoder does not
  implement; `summarize` counts it out of set, never as a pass.
- `ps-open-noext-after-error.adts`, `ps-open-dt-envelope-reference.adts`,
  `ps-open-sbr-after-missing.adts` and `ps-open-sbr-truncated.adts`
  ([aac/ps-open-manifest.json](aac/ps-open-manifest.json)): open Parametric Stereo and
  SBR cases, the manifest's `open_cases`: crafted streams of paths the probes did not
  recover, which `summarize` requires to keep failing; their numbers are in the known
  differences of [crates/codec-aac](../../crates/codec-aac/README.md).
