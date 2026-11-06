# triergon-isobmff

Bounded, incremental reader for ISO base media files: MP4, QuickTime MOV and
fragmented MP4. It is a format-layer crate built on `triergon-core` (input
cursor, budgets, exact time) and uses `triergon-framing` only for AC-3/E-AC-3
configuration in encoded edit plans. It parses boxes, sample tables, fragments,
edit lists and seek indexes, and reads sample payloads by byte range. It does no
I/O of its own, decodes no codec, and does not apply edits to presentation.

The crate exposes two views of the same file:

- **Source view** (`BoxReader`, `SampleTable`, `FragmentIndex`, `SampleReader`,
  seek planners). Every value is a wire fact: original sample ordinals, byte
  ranges, raw composition-offset bits and exact clocks. Unknown clocks stay
  `None`.
- **FFmpeg-compatible view** (types prefixed `Mov9` or `PinnedMovD32`). It
  reproduces the packet order, timestamps, flags and seek behaviour of the
  pinned FFmpeg 9.0 MOV demuxer run with `+noparse+nofillin`
  (`Mov9Profile::PinnedNoParseNoFillin`). It is a separate projection and
  never rewrites source facts.

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
| Movie/track headers | `MovieHeader::parse`, `TrackHeader::parse`, `SampleDescription` |
| Regular sample tables | `SampleTableBuilder` → `SampleTable::{cursor, locate, previous_sync}` |
| Fragments (`moof`/`traf`) | `Trex::parse`, `FragmentBuilder` → `FragmentIndex`, `OwnedFragmentCursor` |
| One track, regular + fragments | `SourceTrack`, `TrackSampleCursor` |
| All tracks in file order | `PhysicalSampleCursor::new_with_media_extents` |
| Payload bytes | `SampleReader::{new, new_terminal_mdat}` |
| Edit lists | `EditList::parse`, `EditEntry::map_rate_one`, `MovieEditLocator` |
| Seek | `SourceSeekPlanner`, `MovieSourceSeekPlanner`, `SourceCompositionLookup` |
| Segment/fragment indexes | `SegmentIndex` (`sidx`), `TrackFragmentRandomAccess` (`tfra`), `MovieFragmentRandomAccessTail` (`mfro`) |
| Recovery groups | `RecoveryDescriptions` (`sgpd`), `RecoveryMappings` (`sbgp`), `RecoveryLookup` |
| Codec configuration | `SampleDescription::extensions`, `AacConfigurationJob`, `check_dts_core_description`, `QtSoundDescriptionV0` |
| FFmpeg-compatible packets | `Mov9PacketCursor`, `Mov9EditCursor`, `Mov9StreamView` |
| FFmpeg-compatible seek | `Mov9NativeIndexCursor`, `Mov9NativeSeekJob`, `Mov9NativeGlobalSeekJob` |

`examples/isobmff_source_trace.rs` drives the source view over a real file and
prints JSON lines (`isobmff_source_trace INPUT [WINDOW<=4096] [WORK_QUANTUM]
[physical]`). `examples/isobmff_consumer_worker.rs` drives the
FFmpeg-compatible operations for comparison runs.

The comparison worker's `--recover-regular-tables` profile uses the bounded
regular recovery cursors for a single local, unedited track. Use it alone
for open and index comparisons. It projects
a zero media timescale and an invalid initial sample description for comparison.
For an initial offset past the source, it checks whether the bounded read
model establishes EOF. The source trace keeps the original fields and
strict validation.

`AacConfigurationJob` reads AudioSpecificConfig from one `mp4a` sample
description, including the supported QuickTime `wave` layout. It returns
the source offset and shared bytes under `ConfigurationLimits` and a
`WorkBudget`. The descriptor identifies MPEG-4 Audio. Callers validate
the bytes and determine the audio object type separately.

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
| FFmpeg-compatible view | AVC, HEVC, AV1, VP9, AAC, AC-3, E-AC-3, DTS (`EncodedEntryKindV1`), raw video, `tx3g`, PCM |

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
  the MOV comparison worker's open and native-index operations require the
  accumulated decode end to be at most `i64::MAX`. Source indexes retain the
  unsigned origin and exact end. Higher starts use the native clock projection.
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

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
