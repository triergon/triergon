# MPEG video picture carriage

Fourteen synthetic transport inputs carry 16×16 pictures, each in an aligned finite
video PES of at most 175 payload bytes with sequence setup and sequence end. Ten contain two
independently timestamped I pictures; four MPEG-2 inputs contain I/P/B pictures
in decode order with I/B/P presentation order, independent PTS/DTS and only the
I picture marked as sync. The predictive headers include forward and backward
vector fields; their slices use intra macroblocks, so these inputs test picture
carriage rather than motion decoding.
Twelve inputs use one span per PES. Two additional inputs split each PES across
two carriers: MPEG-1 TS188 splits the picture start-code prefix after one byte;
MPEG-2 M2TS192 splits it after two bytes. Both retain exact source PTS/DTS and
read through EOF.
The inputs cover MPEG-1 and MPEG-2, TS188 and M2TS192, and 25 and 30000/1001 fps.
The MPEG-2 syntax selects Main Profile / Main Level, progressive 4:2:0 frame
pictures, no repeat fields and default matrices. Every coded macroblock has zero
DC differences and no AC coefficients. The inputs are authored synthetic data
under the repository's MIT OR Apache-2.0 licence.

`cargo xtask fixture transport-mpeg-video --record REFERENCE` generates the tracked
inputs and records complete `ts-au-fold` output from the supplied reference adapter.
For a dynamically linked reference, supply each FFmpeg component with
`--reference-library PATH`; `reference.json` records the worker, adapter and component
hashes. The generator uses the existing PAT/PMT, PES and transport authoring helpers.
It does not invoke the candidate to obtain expectations.

The initial observations use the repository's `harness/reference_ts_worker.c`,
compiled against FFmpeg 9.0.2 public libraries: libavformat 63.1.102, libavcodec
63.1.102 and libavutil 61.1.102. The adapter calls the MPEG-TS demuxer with codec
parsers enabled, `NOFILLIN`, no stream discovery and no decoder. Packet durations
remain zero in this operation. The reader independently derives a source duration
from the sequence header; the comparison keeps that fact separate. The two I pictures in the original inputs
also decode without errors to identical 16×16 gray frames through the public
FFmpeg command-line interface; decoding is not part of the carriage contract.

The dedicated `transport-mpeg-video` reference uses the same pinned git revision
as the container references and enables the MPEG-TS demuxer and ordinary
`mpegvideo` parser. The shared formats configuration is unchanged. Prepare this
profile and build the workers before running the registered comparison:

```sh
CARGO_INCREMENTAL=0 cargo xtask prepare-ffmpeg --profile transport-mpeg-video
CARGO_INCREMENTAL=0 cargo xtask build-workers --only transport-worker
CARGO_INCREMENTAL=0 cargo xtask report \
  --contract harness/features/mpegts-mpeg-video/contract.json \
  --manifest harness/features/mpegts-mpeg-video/corpus.json --corpus .
```

Local packet folds match on all fourteen inputs. Native qualification follows the
[shared acceptance policy](../../../docs/quality.md) on AArch64 and ARMv7 and
remains open for this profile.

Syntax references: ITU-T H.262 (02/2000), Table 6-1, §§6.2.2.1, 6.2.2.3, 6.2.3,
6.2.3.1, 6.2.4 and 6.3.3; ITU-T H.222.0 (10/2014), §2.4.3.6 and Table 2-34.
The reader checks start-code order and selected header facts, not coded block
validity. Additional extensions, MPEG-1 P/B pictures, field pictures, user data,
repeated fields, split or multiple pictures per PES, payloads above 175 bytes and
seeking require separate coverage. The crate README inventories other transport
and subtitle carriages.
