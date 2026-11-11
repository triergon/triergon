# triergon-isobmff

Bounded, incremental reader for ISO base media files: MP4, QuickTime MOV and
fragmented MP4. It is a format-layer crate built on `triergon-core` (input
cursor, budgets, exact time) and uses `triergon-framing` only for AC-3/E-AC-3
configuration in encoded edit plans. It parses boxes, sample tables, fragments,
edit lists and seek indexes, and reads sample payloads by byte range. It does no
I/O of its own, decodes no codec, and does not apply edits to presentation.

`Reader` implements the core `ContainerReader` over the source view, so the crate
works in generic code and inside `triergon::AnyReader`. The parts below stay
available for finer control.

`media_kind(&handler)` classifies an `hdlr` handler code without allocating,
using the same mapping as `TrackInfo::kind`. Unknown handlers stay unknown.

The crate exposes two views of the same file:

- **Source view** (`BoxReader`, `SampleTable`, `FragmentIndex`, `SampleReader`,
  seek planners). Every value is a wire fact: original sample ordinals, byte
  ranges, raw composition-offset bits and exact clocks. Unknown clocks stay
  `None`.
- **FFmpeg-compatible view** (`compat::mov`). It reproduces the packet order,
  timestamps, flags and seek behaviour of FFmpeg's MOV demuxer run with
  `+noparse+nofillin` (`DemuxProfile::NoParseNoFillin`), for a consumer that
  chooses FFmpeg's result. It is a separate projection and never rewrites source
  facts. The `compat::mov` documentation names the FFmpeg revision it reproduces.

Values that only make sense when comparing traces with FFmpeg, such as its return
codes, log lines and models of its input layer, are compiled only with the
non-default `reference-trace` feature, under `reference_trace::mov`.

After packet traversal reaches the end, `replay_track` can build an index for a
selected track using the sample extents and clock facts already checked during
that traversal. Each replay has its own entry limit.

## Usage

All parsers are polled. Each `poll` takes a `WorkBudget` (and usually a cancel
flag) and returns `Yield`, a result, or `Step::NeedInput` for the shared
`InputCursor` to satisfy.

`SampleReader::new_with_shared_window` can return a view of a complete sample
in an existing shared input window. The view retains the window's budget charge.
Random access can reuse bytes already supplied in the same generation. Samples
split across windows use a bounded packet buffer.
Complete indexed sources can check all packet projections by track before
rewinding for merged playback.
Short regular tracks can keep extra seek restart points in the existing
checkpoint allocation.
Complete fragment-only tracks can also retain a restart for backward seek
when their projected decode times increase and each run begins at a sync sample.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, WorkBudget,
};
use triergon_isobmff::{
    BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, Limits, SampleReader, SampleStep,
    SampleTableBuilder,
};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/r-multitrack.mov");
let file = std::fs::read(path)?; // any transport; the parsers do no I/O
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(file.len() as u64), random_access: true };
let mut input = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
let cancel = AtomicBool::new(false);
let mut work = WorkBudget::new(1 << 16);
let supply = |input: &mut InputCursor, req: InputRequest| -> triergon_core::Result<()> {
    match file.get(usize::try_from(req.offset).map_err(|_| Error::Overflow)?..) {
        Some(rest) if !rest.is_empty() => {
            let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
            input.provide_owned(InputWindow::new(req.source, req.token, req.offset, bytes)?)
        }
        _ => input.finish(req.source, req.token),
    }
};

// Top-level boxes: retain `moov`, note where `mdat` holds the media.
let mut boxes = BoxReader::new(&input, 16 << 20)?;
let (mut moov, mut mdat) = (None, None);
loop {
    match boxes.poll(&mut input, &mut work, &cancel)? {
        Step::Ready(BoxEvent::Header(h)) if &h.kind == b"moov" => boxes.read_metadata(&input)?,
        Step::Ready(BoxEvent::Header(_)) => boxes.skip(&mut input)?,
        Step::Ready(BoxEvent::Metadata(raw)) => moov = Some(raw),
        Step::Ready(BoxEvent::Skipped(h)) if &h.kind == b"mdat" => mdat = Some(h.payload_range()?),
        Step::Ready(BoxEvent::Skipped(_)) => {}
        Step::NeedInput(req) => supply(&mut input, req)?,
        Step::Yield => work = WorkBudget::new(1 << 16),
        Step::End(_) => break,
    }
}

// The first track's `stbl`, indexed without reading any media.
let child = |parent: &BoxView, kind: &[u8; 4]| -> triergon_core::Result<BoxView> {
    let mut children = parent.children(0, Limits::default())?;
    loop {
        match children.poll(&mut WorkBudget::new(1 << 16))? {
            BoxStep::Box(b) if &b.header().kind == kind => return Ok(b),
            BoxStep::Box(_) | BoxStep::Yield => {}
            BoxStep::End => return Err(Error::Invalid("missing box")),
        }
    }
};
let trak = child(&moov.ok_or("no moov")?, b"trak")?;
let stbl = child(&child(&child(&trak, b"mdia")?, b"minf")?, b"stbl")?;
let mut builder = SampleTableBuilder::new(stbl, pool.clone(), Limits::default())?;
while builder.poll(&mut work)? == BuildStatus::Yield {
    work = WorkBudget::new(1 << 16);
}
let table = builder.finish()?;

// Every sample: timing from the table, payload read by byte range.
let mdat = mdat.ok_or("no mdat")?;
let mut samples = table.cursor();
loop {
    let sample = match samples.poll(&mut work)? {
        SampleStep::Sample(sample) => sample,
        SampleStep::Yield => { work = WorkBudget::new(1 << 16); continue }
        SampleStep::End => break,
    };
    let mut read = SampleReader::new(&mut input, sample, mdat.clone(), 1 << 20)?;
    let packet = loop {
        match read.poll(&mut input, &mut work, &cancel)? {
            Step::Ready(packet) => break packet,
            Step::NeedInput(req) => supply(&mut input, req)?,
            Step::Yield => work = WorkBudget::new(1 << 16),
            Step::End(_) => return Err("source ended inside a sample".into()),
        }
    };
    // packet.sample: ordinal, dts_ticks, cts_ticks, is_container_sync(); packet.payload
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`examples/isobmff_packets.rs FILE` does this for every track and prints the samples
of all tracks in file order through `PhysicalSampleCursor`.

| Task | Entry points |
| --- | --- |
| Box framing | `BoxReader`, `BoxView::children`, `BoxChildren` |
| Movie/track headers | `MovieHeader::parse`, `MovieHeader::declared_duration_ticks`, `TrackHeader::parse`, `SampleDescription` |
| Regular sample tables | `SampleTableBuilder` → `SampleTable::{cursor, locate, previous_sync}` |
| Fragments (`moof`/`traf`) | `Trex::parse`, `FragmentBuilder` → `FragmentIndex`, `OwnedFragmentCursor` |
| One track, regular + fragments | `SourceTrack`, `TrackSampleCursor` |
| All tracks in file order | `PhysicalSampleCursor::new_with_media_extents` |
| Payload bytes | `SampleReader::{new, new_terminal_mdat}` |
| Edit lists | `EditList::parse`, `EditEntry::map_rate_one`, `MovieEditLocator` |
| Seek | `SourceSeekPlanner`, `MovieSourceSeekPlanner`, `SourceCompositionLookup` |
| Segment/fragment indexes | `SegmentIndex` (`sidx`), `TrackFragmentRandomAccess` (`tfra`), `MovieFragmentRandomAccessTail` (`mfro`) |
| Recovery groups | `RecoveryDescriptions` (`sgpd`), `RecoveryMappings` (`sbgp`), `RecoveryLookup` |
| Codec configuration | `SampleDescription::extensions`, `AacConfigurationJob`, `ObjectTypeScan`, `check_dts_core_description`, `QtSoundDescriptionV0` |
| Whole movie | `open::MovieOpen` → `open::Movie`, under `open::OpenLimits`; `new_progressive` stops at the first media after a front movie, `new_recovery` admits terminal media declarations |
| Neutral reader | `Reader`, `ReaderLimits`, `Track`, `Packet`, `ReaderEvent` |
| FFmpeg-compatible packets | `compat::mov::packets::{PacketCursor, StreamView}`, `compat::mov::edits::EditCursor` |
| FFmpeg-compatible seek | `compat::mov::seek::{IndexCursor, SeekJob, GlobalSeekJob}` |
| FFmpeg-compatible recovery | `compat::mov::recovery::{RecoveryTrackHeader, RunRecoveryBuilder, PartialSampleReader}` |
| Encoded tracks | `compat::mov::encoded::EncodedStreamJob`, `compat::mov::edits::EncodedEditJob` |

## Container reader

`Reader::new` takes a seekable source, runs `open::MovieOpen` over it and then
delivers samples in ascending byte order across tracks, each track in decode order.
Every step takes a `WorkBudget` and the reader asks for input with `Step::NeedInput`,
as any `ContainerReader` does. It retains `moov` and each `moof`, under `ReaderLimits`,
and shares a sample's window where the whole sample fits in one.

| Neutral item | ISO BMFF source |
|---|---|
| `TrackId` | `track_ID` |
| `MediaKind` | `hdlr` handler; an unlisted handler is `MediaKind::Unknown` |
| `codec_tag` | The first sample entry code |
| `codec` | One mapping from handler and entry code, plus the object type `esds` states for `mp4a`. Anything else is `CodecId::Other` with the entry code |
| `configuration` | The payload of `avcC`, `hvcC`, `av1C`, `vpcC`, `dac3`, `dec3` or `ddts`; AudioSpecificConfig for AAC; empty if the record is absent; the whole sample entry payload for any other entry |
| `language` | The ISO 639-2/T code in `mdhd`; `None` for a QuickTime language code |
| `pts`, `dts`, `duration` | Ticks of the media timescale, from the sample tables or `trun` and `tfdt`; the duration is the decode delta |
| `random_access` | `stss` or fragment sample flags |
| `Event::Other` | `ReaderEvent::Box` for each root box, `ReaderEvent::Movie` once tracks are available |

Dolby Vision sample entries `dvh1` and `dvhe` report HEVC and `dav1` reports AV1.
The reader and range playback keep the source entry code and expose the base
`hvcC` or `av1C` configuration. Dolby Vision boxes remain available through
`SampleDescription::extensions`. See the [registered sample entries](https://mp4ra.org/registered-types/codecs).

`request_seek` finds the latest stated sync sample presented at or before the target
on the track's media clock, or the first one if none is, without reading media. It
reports `SeekMethod::Index`. Other tracks resume at their first sample at or after
that time, which may not be a sync sample.

What the reader does not do:

- **Forward-only sources** are refused, because `mdat` can precede the metadata.
- **Edit lists** are not applied. Times are media times and the list is on
  `Track::edits`; `compat::mov` gives FFmpeg's edited packets.
- **A sample entry change inside a track** is not reflected in `TrackInfo`, which
  describes the first entry. `Packet::sample` names each sample's entry and
  `Track::descriptions` keeps them all.
- **Colour, chapters and other metadata** have no typed field here. They stay
  readable through `Track::descriptions` and `Reader::movie_box`, so `colour` is
  `None`, and `default_track` and `forced` are `None` because the format has no
  such flags.
- **External data references** are refused, as is media outside an `mdat`. Only a
  `url ` entry flagged self-contained counts as this file.
- **An empty `traf`** fails the open, because it cannot state the base the next
  `traf` may inherit.
- **A fragment without `tfdt`** has `None` times, and a seek cannot land on it.
- **Times beyond `i64`** fail the read with `Error::Overflow`.
- **Encrypted entries** report their outer code (`encv`, `enca`) as `CodecId::Other`.
- **A QuickTime `mp4a` entry** whose sound description or `wave` layout cannot be walked
  stays `CodecId::Other`, as does any object type other than AAC or DTS with a core.

`examples/isobmff_consumer_worker.rs` drives the FFmpeg-compatible operations for
comparison runs and needs the `reference-trace` feature.

The comparison worker's `--recover-regular-tables` profile uses the bounded
regular recovery cursors for a single local, unedited track. Use it alone
for open and index comparisons. It projects
a zero media timescale and an invalid initial sample description for comparison.
For an initial offset past the source, it checks whether the bounded read
model establishes EOF. The source trace keeps the original fields and
strict validation.

`AacConfigurationJob` reads AudioSpecificConfig from one `mp4a` sample
description, including QuickTime version 1 and version 2 sound descriptions
with `wave` extensions. For version 2, it checks the declared extension offset
and reads the descriptor inside `wave`. It returns the source offset and shared
bytes under `ConfigurationLimits` and a `WorkBudget`. The descriptor identifies
MPEG-4 Audio. Callers validate the bytes and determine the audio object type separately.
`ObjectTypeScan` reads any `mp4a` or `mp4s` description's stated object
type and decoder-specific information the same way.

Range playback accepts edit lists for indexed AAC-LC with 1024 core samples and
an explicit Program Configuration Element of up to eight channels. The edit plan
keeps the sample durations in `stts` and uses the coded frame length for trims.

`BoxReader::with_media_data_policy` can preserve a terminal `mdat`'s
declared extent when the source length is known and the media ends early.
This lets a compatibility consumer validate sample ranges against the
declaration and read only the available prefix. `BoxReader::new` still
rejects boxes extending past the source.

## Supported

| Area | Coverage |
| --- | --- |
| Box framing | 32-bit and 64-bit sizes, size-zero final box, u64 endpoints, bounded child walks |
| Sample tables | `stts`, `ctts` v0/v1 (signed), `stsc`, `stsz`, `stz2` (4/8/16-bit), `stco`, `co64`, `stss`, `stps`, `sdtp` |
| Sample entries | Visual, QuickTime sound v0, `tx3g` extension walks; raw entry bytes always retained. `extensions` refuses sound v1/v2 layouts. |
| Fragments | `trex`, `tfhd` defaults and flags, `trun` v0/v1 (signed CTO), `tfdt` v0/v1, moof-relative base |
| Edit lists | `elst` v0/v1; empty edits; rate-one point mapping |
| Seek and indexes | sync/preroll plans, movie-time and composition targets, `sidx`, `tfra`/`mfro`, `roll`/`prol` groups |
| Codec configuration | AAC `AudioSpecificConfig` (incl. QuickTime `wave`), DTS core signalling, AC-3/E-AC-3, opaque `avcC`/`hvcC`/`av1C`/`vpcC` |
| QuickTime PCM | v0 unsigned 8-bit and signed 16-bit big-endian (`QtPcmSampleReaderV0`) |
| FFmpeg-compatible view | AVC, HEVC, AV1, VP9, AAC, AC-3, E-AC-3, DTS (`compat::mov::encoded::EncodedEntryKind`), raw video, `tx3g`, PCM |

## Behaviour and limits

- **Input.** The caller owns I/O through the core `InputCursor`. `BoxReader`
  retains only boxes the caller asks for with `read_metadata`; `mdat` is never
  retained. A reader is bound to one input transaction: after a reset, build a
  new reader.
- **Bounds.** `Limits` caps boxes, table records and sample descriptions
  (defaults 1,000,000 / 1,000,000 / 256). All storage is charged to the shared
  `Budget`. Tables stay as compact wire runs; nothing allocates per sample.
  Work is charged as fields are parsed, so every operation can yield and be
  cancelled.
- **Clocks.** `Sample` carries exact source ticks; `Sample::source_times`
  converts to core `Time`. A fragment with no `tfdt` and no caller-supplied
  `FragmentContext::decode_origin` has unknown DTS/CTS, not zero.
  `CompositionOffset` keeps version, raw bits and the signed reading separate;
  an absent offset differs from an explicit zero.
- **Comparison decode runs.** For an explicit `tfdt` at or below `i64::MAX`,
  `reference_trace::mov::fragment_clock::check_signed_decode_runs` requires the
  accumulated decode end to be at most `i64::MAX`. The MOV comparison worker applies
  it to open and index operations. Source indexes retain the unsigned origin and
  exact end. Higher starts stay on `compat::mov::clock::ClockField`.
- **Comparison sample entries.**
  `reference_trace::mov::sample_entry::RawVisualEntriesJob` checks configuration
  boxes in raw visual entries. The MOV comparison worker runs it for every track
  before publishing streams, for open, index and seek. A nonempty Opus configuration
  shorter than 11 payload bytes fails the operation. Source inspection retains the
  original entry and its child boxes.
- **Surplus timing entries.** `SampleTableBuilder::with_timing_policy`
  accepts `TimingRunPolicy::BoundedPrefix` for the QuickTime-compatible
  interpretation of overlong timing runs. The last decode run may cross
  the sample count if it starts within it. Positive composition runs may
  extend beyond it. The sample table retains all raw runs and reports the
  unused counts through `SampleTable::timing_run_surplus`. The source
  seek planners and default physical cursor constructors require exact
  coverage. Compatibility playback can opt into physical range validation
  for a checked prefix, including forward-only media, through the physical
  cursor constructors that accept a timing policy.
- **Out-of-range sync entries.** `SampleTableBuilder::with_policy` can select
  `SyncSamplePolicy::BoundedPrefix`. The first `stss` entry past the last
  sample ends the usable prefix. That entry and the rest of the table remain
  available as raw metadata, with their count in
  `SampleTable::unused_sync_entries`. They never mark a sample or provide a
  seek point. The default policy rejects an out-of-range entry.
- **Payload ranges.** `SampleReader::new` requires the sample to lie inside the
  caller's `mdat` payload extent. `PhysicalSampleCursor::new_with_media_extents`
  checks every track's ranges against the `mdat` extents before emitting
  anything. Valid but backward-ordered layouts are `Unsupported` for the
  physical cursor only.
- **Edits.** Source edit lists are parsed and mapped at rate one only.
  Non-unit rates and dwell edits are refused. The source view never repeats
  or shifts packets; the FFmpeg-compatible view does.
  `SampleTable::has_composition_offsets` reports a regular table's `ctts`
  presence before reading its samples.
  Range playback preserves AAC sample durations from `stts`, including
  durations longer than one coded frame. Trim counts use the coded frame length.
  The regular range view also establishes a 1024-sample clock for indexed AAC-LC
  configurations 11, 12 and 14 when the media timescale matches the core rate.
  The compatible view of a complete fragmented file supports one open-ended
  rate-one media edit after an optional leading empty edit. The empty edit
  delays packet clocks by its duration, rounded to the track time base.
  Backward seek queries subtract the media start. AAC leading trim for
  a single initialization edit uses the configured audio sample rate.
  Regular Opus range playback retains the `dOps` record and exposes its canonical
  identification header. The MOV policy projects leading and trailing edit trim
  and retains up to one second of history plus the preceding sample for a long
  leading crop. Source packet durations stay available alongside the projected
  durations. A secondary Opus seek keeps an exact match or searches 10 ms earlier
  when the target falls between samples. The optional `with_opus_recovery` policy
  starts from the Opus track's source `roll` group at the video presentation
  landing. It reports the first source sample and available preroll, including
  when recovery starts at the beginning of the track. It refuses a landing at
  or past the end of the Opus presentation window.
- **Seek.** Seek planners select a preceding container sync sample, record
  available and missing preroll, and validate every media range before
  publishing a plan. A sync sample is a container fact, not proof of decoder
  recovery. `sidx` and `tfra` entries are hints and are not verified against
  the media.
- **Refusals.** Unknown box versions, unknown flags, unselected `stz2` widths,
  non-unit edits and unobserved FFmpeg-compatible profiles return
  `Error::Unsupported`. Structural damage returns `Invalid` or `Truncated`.
  Readers keep a failure sticky; build a new reader to continue.
- **Not handled.** External data references (`dref` URLs), encryption,
  metadata (`udta`/`meta`) interpretation, decoder recovery and muxing.

## Conformance

Specification basis:

- Apple QuickTime File Format; Adobe F4V 10.1 §§1.3, 2.4–2.16 (boxes, tables,
  fragments, `tfra`/`mfro`).
- ETSI TS 126 244 V18.0.0 §13.4 (`sidx`) and §13.5 (`tfdt`).
- ISO/IEC 14496-14:2020 §6.7 (sample entries and elementary stream descriptors).
- ISO/IEC 14496-12:2008/Amd1:2009 §§8.6.1.3.1–.3 (signed `ctts`);
  14496-12:2008/FDAmd2 §8.8.7.1 (moof-relative base).
- MPEG m76694 Table 9 and FDAM3 draft text for `stz2`, signed `trun` and
  `roll`/`prol`. These are draft sources: the code treats them as
  experimental and refuses fields they leave open.
- ETSI TS 102 366 Annex F (AC-3/E-AC-3 in ISO media).

Verification:

- Integration tests under `tests/` use independently authored fixtures in
  `tests/fixtures/` with expected values written from the specifications.
  Fixture hashes and provenance are in `tests/fixtures/PROVENANCE.json`;
  FFmpeg-generated fixtures document their commands (for example
  [range-encoded-edits](tests/fixtures/range-encoded-edits/README.md)).
- The FFmpeg-compatible view is compared packet by packet (order, timestamps,
  flags, bytes) with the pinned FFmpeg reference. Comparisons never sort or
  normalise either side.

With the `formats` reference prepared, `cargo xtask build-workers` registers the
comparison workers. Run `cargo xtask isobmff-bench` for all 1,020 open and index
parity checks, covering 972 output labels and 48 explicit refusals. Add
`--benchmark --memory` to measure the complete selection, or
`--benchmark --memory --subset52` for the separate 52-operation selection.
Both retain all 1,020 parity checks. The complete selection measures 956 physical
operations with 16 explicit aliases. Run these commands with `CARGO_INCREMENTAL=0`
and keep the RSS kernel module unloaded while timing.

Use `--benchmark --memory --require-acceptance` for a combined native AArch64 Linux
run, with `TRIERGON_RSS_KERNEL_BOUND` unset or `0`. Each case completes timing
before memory on the same recorded worker pair. The conservative RSS observer
must pass the strict memory gates. Final report assembly streams one case's raw
observations at a time and keeps the individual JSON files and hashes alongside it.
Runtime and memory verdicts use the shared harness acceptance evaluator and its
current policy. Each collection also writes a compact `collection.json` that
references hash-checked samples. Join separate timing and memory collections with:

```sh
cargo xtask grade TIMING/collection.json MEMORY/collection.json \
    --contract harness/features/isobmff-open-index/contract.json \
    --manifest harness/features/isobmff-open-index/corpus.json --output DIR
```

The grader checks the complete corpus, exact outputs for each role, refusal
diagnostics and alias operations. It reads one case's raw observations at a time;
the 52-operation selection cannot qualify the complete feature.

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
