# triergon-isobmff

Bounded, incremental reader for ISO base media files: MP4, QuickTime MOV and
fragmented MP4. It is a format-layer crate built on `triergon-core` (input
cursor, budgets, exact time) and uses `triergon-framing` only for AC-3/E-AC-3
configuration in encoded edit plans. It parses boxes, sample tables, fragments,
edit lists and seek indexes, and reads sample payloads by byte range. It does no
I/O of its own, decodes no codec, and does not apply edits to presentation.

`Reader` implements the core `ContainerReader` over the source view, so the crate
works in generic code and inside `triergon::AnyReader`. The parts below stay
available for finer control. The crate exposes two views of the same file:

- **Source view** (`BoxReader`, `SampleTable`, `FragmentIndex`, `SampleReader`,
  seek planners). Every value is a wire fact: original sample ordinals, byte
  ranges, raw composition-offset bits and exact clocks. Unknown clocks stay
  `None`.
- **FFmpeg-compatible view** (`compat::mov`). It reproduces the packet order,
  timestamps, flags and seek behaviour of FFmpeg's MOV demuxer run with
  `+noparse+nofillin` (`DemuxProfile::NoParseNoFillin`), for a consumer that
  chooses FFmpeg's result. It is a separate projection and never rewrites source
  facts. The `compat::mov` documentation names the FFmpeg revision it reproduces.

## Cargo features

- `reference-trace` (off by default): values that only make sense when comparing
  traces with FFmpeg, such as its return codes, log lines and models of its input
  layer, under `reference_trace::mov`. `examples/isobmff_consumer_worker.rs` needs it.

## Usage

All parsers are polled. Each `poll` takes a `WorkBudget` (and usually a cancel
flag) and returns `Yield`, a result, or `Step::NeedInput` for the shared
`InputCursor` to satisfy.

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
| Payload bytes | `SampleReader::{new, new_with_shared_window, new_terminal_mdat}` |
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

`SampleReader::new_with_shared_window` returns a view of a complete sample in an
existing shared input window and retains the window's budget charge; samples split
across windows use a bounded packet buffer.

## Container reader

`Reader::new` takes a seekable source, runs `open::MovieOpen` over it and then
delivers samples in ascending byte order across tracks, each track in decode order.
It retains `moov` and each `moof` under `ReaderLimits` (largest sample 64 MiB by
default) and shares a sample's window where the whole sample fits in one.

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

Dolby Vision sample entries `dvh1` and `dvhe` report HEVC and `dav1` reports AV1,
with the base `hvcC` or `av1C` configuration; the Dolby Vision boxes remain
available through `SampleDescription::extensions`. See the
[registered sample entries](https://mp4ra.org/registered-types/codecs).

`request_seek` finds the latest stated sync sample presented at or before the target
on the track's media clock, or the first one if none is, without reading media. It
reports `SeekMethod::Index`. Other tracks resume at their first sample at or after
that time, which may not be a sync sample.

Limits of the reader:

- Forward-only sources are refused, because `mdat` can precede the metadata.
- Edit lists are not applied: times are media times and the list is on
  `Track::edits`. `compat::mov` gives FFmpeg's edited packets.
- `TrackInfo` describes the first sample entry; `Packet::sample` names each
  sample's entry and `Track::descriptions` keeps them all.
- For known visual entries, `colour` reads the first valid `nclx` or `nclc`
  colour box in the first sample entry. It preserves the numeric codes and
  reports an absent range for `nclc`. The raw descriptions retain the boxes.
  Chapters and other metadata remain available through `Reader::movie_box`.
- External data references and media outside an `mdat` are refused; only a `url `
  entry flagged self-contained counts as this file. An empty `traf` fails the open.
- A fragment without `tfdt` has `None` times and cannot be a seek landing. Times
  beyond `i64` fail with `Error::Overflow`.
- Encrypted entries (`encv`, `enca`), unwalkable QuickTime `mp4a` layouts and object
  types other than AAC or DTS with a core report `CodecId::Other`.

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

`AacConfigurationJob` returns the AudioSpecificConfig bytes of an `mp4a` entry
(including QuickTime v1/v2 `wave`) unvalidated; callers determine the object type.

## Behaviour and limits

- **Input.** `BoxReader` retains only boxes the caller asks for with
  `read_metadata`; `mdat` is never retained. A reader is bound to one input
  transaction: after a reset, build a new reader. `BoxReader::new` rejects boxes
  extending past the source; `with_media_data_policy` can keep a terminal `mdat`'s
  declared extent when the media ends early.
- **Bounds.** `Limits` caps boxes, table records and sample descriptions
  (defaults 1,000,000 / 1,000,000 / 256). All storage is charged to the shared
  `Budget`; tables stay as compact wire runs and nothing allocates per sample.
  Every operation can yield and be cancelled.
- **Clocks.** `Sample` carries exact source ticks; `Sample::source_times`
  converts to core `Time`. A fragment with no `tfdt` and no caller-supplied
  `FragmentContext::decode_origin` has unknown DTS/CTS, not zero.
  `CompositionOffset` keeps version, raw bits and the signed reading separate;
  an absent offset differs from an explicit zero.
- **Table policies.** By default timing runs must cover the sample count exactly
  and an out-of-range `stss` entry is refused. `SampleTableBuilder` can opt into
  `TimingRunPolicy::BoundedPrefix` (QuickTime-style overlong runs, reported by
  `SampleTable::timing_run_surplus`) and `SyncSamplePolicy::BoundedPrefix` (unused
  entries counted by `SampleTable::unused_sync_entries`, never seek points). The
  source seek planners and default physical cursors require exact coverage.
- **Payload ranges.** `SampleReader::new` requires the sample inside the caller's
  `mdat` payload extent; `PhysicalSampleCursor::new_with_media_extents` checks every
  track first and returns `Unsupported` for valid backward-ordered layouts. Physical
  order and per-track decode order cannot both be maintained in such a track. After
  this refusal, `resume_in_track_order` can traverse the validated samples in track
  order; payload access may require backward range requests. It cannot resume when
  media validation was deferred.
- **Edits.** Source edit lists are parsed and mapped at rate one only; non-unit
  rates and dwell edits are refused. The source view never repeats or shifts
  packets; the FFmpeg-compatible view does, including AAC and Opus trim for range
  playback and, for fragmented files, one open-ended rate-one media edit after an
  optional leading empty edit.
- **Seek.** Seek planners select a preceding container sync sample, record
  available and missing preroll, and validate every media range before publishing
  a plan. A sync sample is a container fact, not proof of decoder recovery. `sidx`
  and `tfra` entries are hints, not verified against the media.
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

Fixture hashes and provenance are in `tests/fixtures/PROVENANCE.json`;
FFmpeg-generated fixtures document their commands (for example
[range-encoded-edits](tests/fixtures/range-encoded-edits/README.md)).
Verification: the FFmpeg comparisons are defined in
[harness/features/isobmff-reader](../../harness/features/isobmff-reader),
[isobmff-open-index](../../harness/features/isobmff-open-index) and
[isobmff-seek](../../harness/features/isobmff-seek); results are in
[docs/status.md](../../docs/status.md). The reader comparison includes first-entry
source colour on the `nclc` and `nclx` fixtures. It excludes colour on the
configuration-change fixture because ffprobe aggregates a later entry.
