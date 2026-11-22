# triergon-isobmff

Bounded, incremental reader for ISO base media files: MP4, QuickTime MOV and
fragmented MP4. It parses boxes, sample tables, fragments, edit lists, sample groups,
segment and fragment indexes and codec configuration records, plans seeks, and reads
sample payloads by byte range. It does no I/O of its own, decodes no codec and does
not apply edits to presentation. It is built on `triergon-core` (input cursor,
budgets, exact time) and uses `triergon-framing` only for AC-3/E-AC-3 configuration
in encoded edit plans.

| Area | Coverage |
| --- | --- |
| Box framing | 32-bit and 64-bit sizes, size-zero final box, u64 endpoints, bounded child walks |
| Sample tables | `stts`, `ctts` v0/v1 (signed), `stsc`, `stsz`, `stz2` (4/8/16-bit), `stco`, `co64`, `stss`, `stps`, `sdtp` |
| Sample entries | Visual, QuickTime sound v0, `tx3g` extension walks; raw entry bytes always retained. `extensions` refuses sound v1/v2 layouts |
| Fragments | `trex`, `tfhd` defaults and flags, `trun` v0/v1 (signed CTO), `tfdt` v0/v1, moof-relative base |
| Edit lists | `elst` v0/v1; empty edits; rate-one point mapping |
| Seek and indexes | sync/preroll plans, movie-time and composition targets, `sidx`, `tfra`/`mfro`, `roll`/`prol` groups |
| Codec configuration | AAC `AudioSpecificConfig` (incl. QuickTime `wave`), DTS core signalling, AC-3/E-AC-3, opaque `avcC`/`hvcC`/`av1C`/`vpcC` |
| QuickTime PCM | v0 unsigned 8-bit and signed 16-bit big-endian (`PcmSampleReader`) |
| FFmpeg-compatible view | AVC, HEVC, AV1, VP9, AAC, AC-3, E-AC-3, DTS (`compat::mov::encoded::EncodedEntryKind`), raw video, `tx3g`, PCM |

## Reading packets

`Reader` is the way to open a file and read its packets. It implements the core
`ContainerReader`, so it works in generic code and inside `triergon::AnyReader`.

Every packet of a file, through `Reader`:

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::container::{ContainerReader, Event, PacketInfo};
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_isobmff::{Reader, ReaderLimits};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/r-multitrack.mov");
let file = std::fs::read(path)?; // any transport; the reader does no I/O
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(file.len() as u64), random_access: true };
let mut reader = Reader::new(&pool, SourceId(1), caps, ReaderLimits::default(), 64 * 1024)?;
let cancel = AtomicBool::new(false);
let mut bytes_read = 0;
loop {
    match reader.step(&mut WorkBudget::new(1 << 16), &cancel)? {
        // packet.track(), packet.pts(), packet.dts(), packet.random_access(), packet.sample()
        Step::Ready(Event::Packet(packet)) => bytes_read += packet.payload().len(),
        Step::Ready(_) => {} // root boxes, tracks becoming available, seek receipts
        Step::NeedInput(req) => match file.get(usize::try_from(req.offset)?..) {
            Some(rest) if !rest.is_empty() => {
                let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                reader.provide(&InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            }
            _ => reader.finish(req.source, req.token)?,
        },
        Step::Yield => {} // step budget spent
        Step::Backpressure(_) => {} // drop retained packets, then step again
        Step::End(_) => break,
    }
}
assert!(bytes_read > 0 && !reader.tracks().is_empty());
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Reader::new` needs a seekable source, because `mdat` may precede the metadata. It
walks the root boxes with `open::MovieOpen`, indexes every track, and then delivers
samples in ascending byte order across tracks, each track in decode order. It retains
`moov` and each `moof` under `ReaderLimits`, and shares a sample's input window where
the whole sample fits in one. `request_seek` lands the track on the latest stated sync
sample presented at or before the target on its media clock, or the first one if none
is, without reading media (`SeekMethod::Index`); the other tracks resume at their first
sample at or after that time, which may not be a sync sample.

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

Dolby Vision entries `dvh1` and `dvhe` report HEVC and `dav1` reports AV1, with the
base `hvcC` or `av1C` as configuration; the Dolby Vision boxes stay readable through
`SampleDescription::extensions`. `media_kind(&handler)` gives the same handler mapping
as `Track`'s kind without allocating.

The reader does not apply edit lists: times are media times and the list is on
`Track::edits`. It refuses forward-only sources, external data references (only a
`url ` entry flagged self-contained counts as this file), media outside an `mdat`
and an empty `traf`, which cannot state the base the next `traf` may inherit. A
fragment without `tfdt` has unknown times and a seek cannot land on it; times beyond
`i64` fail with `Error::Overflow`. `TrackInfo` describes a track's first sample entry:
`Packet::sample` names each sample's entry and `Track::descriptions` keeps them all.
Colour, chapters and other metadata have no typed field and stay readable through
`Track::descriptions` and `Reader::movie_box`. Encrypted entries (`encv`, `enca`), a
QuickTime `mp4a` entry whose sound description or `wave` layout cannot be walked, and
an object type other than AAC or DTS with a core report `CodecId::Other`.

## Lower-level tools

The modules are the parts `Reader` is built from, for a caller that needs a table, a
fragment or a seek plan on its own. Each is polled with a `WorkBudget` (and usually a
cancel flag), so every operation can yield and be cancelled. Readers of the input
return the core `Step`, whose `NeedInput` the shared `InputCursor` satisfies; walks
over parsed boxes and tables return `Next` (`Item`, `Yield`, `End`); builders return
`BuildStatus` and are then finished; cursors whose items are taken separately return
`CursorStatus`.

| Task | Entry points |
| --- | --- |
| Box framing | `boxes::{BoxReader, BoxView, BoxChildren}` |
| Movie/track headers | `movie::{MovieHeader, TrackHeader, SampleDescription}` |
| Regular sample tables | `table::SampleTableBuilder` → `table::SampleTable::{cursor, locate, previous_sync}` |
| Fragments (`moof`/`traf`) | `fragment::{Trex, FragmentBuilder}` → `fragment::{FragmentIndex, OwnedFragmentCursor}` |
| One track, regular + fragments | `interleave::{SourceTrack, TrackSampleCursor}` |
| All tracks in file order | `interleave::PhysicalSampleCursor::new_with_media_extents` |
| Payload bytes | `payload::SampleReader::{new, new_terminal_mdat}` |
| Edit lists | `movie::{EditList, EditEntry}`, `seek::MovieEditLocator` |
| Seek | `seek::{SourceSeekPlanner, MovieSourceSeekPlanner, SourceCompositionLookup}` |
| Segment/fragment indexes | `index::{SegmentIndex, TrackFragmentRandomAccess, MovieFragmentRandomAccessTail}` (`sidx`, `tfra`, `mfro`) |
| Recovery groups | `sample_groups::{RecoveryDescriptions, RecoveryMappings, RecoveryLookup}` (`sgpd`, `sbgp`) |
| Codec configuration | `SampleDescription::extensions`, `config::{AacConfigurationJob, ObjectTypeScan, check_dts_core_description}` |
| QuickTime sound | `quicktime::{SoundDescriptionV0, PcmSampleReader}` |
| Whole movie | `open::MovieOpen` → `open::Movie`, under `open::OpenLimits`; `new_progressive` stops at the first media after a front movie, `new_recovery` admits terminal media declarations |
| Neutral reader | `Reader`, `ReaderLimits`, `Track`, `Packet`, `ReaderEvent` |
| FFmpeg-compatible packets | `compat::mov::packets::{PacketCursor, StreamView}`, `compat::mov::edits::EditCursor` |
| FFmpeg-compatible seek | `compat::mov::seek::{IndexCursor, SeekJob, GlobalSeekJob}` |
| FFmpeg-compatible recovery | `compat::mov::recovery::{RecoveryTrackHeader, RunRecoveryBuilder, PartialSampleReader}` |
| Encoded tracks | `compat::mov::encoded::EncodedStreamJob`, `compat::mov::edits::EncodedEditJob` |

The same file through the modules: its root boxes, one track's sample table, and that
track's samples read by byte range. `examples/isobmff_packets.rs FILE` does this for
every track and prints the samples of all tracks in file order through
`PhysicalSampleCursor`.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId,
    Step, WorkBudget,
};
use triergon_isobmff::{
    BuildStatus, Limits, Next,
    boxes::{BoxEvent, BoxReader, BoxView},
    payload::SampleReader,
    table::SampleTableBuilder,
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
            Next::Item(b) if &b.header().kind == kind => return Ok(b),
            Next::Item(_) | Next::Yield => {}
            Next::End => return Err(Error::Invalid("missing box")),
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
        Next::Item(sample) => sample,
        Next::Yield => { work = WorkBudget::new(1 << 16); continue }
        Next::End => break,
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

Choices the modules leave to the caller:

- `SampleReader::new` copies a sample from the current window where it holds the
  sample's start, and `new_with_shared_window` returns a view of a complete sample in
  a shared window, which keeps the window's budget charge. `BoxReader::skip` continues
  from the current window where the skipped box ends inside it.
- `BoxReader::with_media_data_policy` keeps a terminal `mdat`'s declared extent when
  the source length is known and the media ends early, so that sample ranges can be
  checked against the declaration and only the available prefix read.
- `SampleTableBuilder::with_policy` can accept `stts` and `ctts` runs that count more
  samples than the size table (`TimingRunPolicy::BoundedPrefix`: only the last decode
  run may cross the sample count, and only if it starts within it) and `stss` entries
  past the last sample (`SyncSamplePolicy::BoundedPrefix`: the first such entry ends
  the usable prefix). The raw runs stay in the table, and `timing_run_surplus` and
  `unused_sync_entries` count what was not used.
- `MovieOpen::new_progressive` stops at the first media after a front movie, and
  `new_recovery` accepts terminal media declarations, the legacy QuickTime
  `dhlr`/`cios` component under an `mhlr` media handler, and an `sdtp` with flag 1 at
  version 0.
- `AacConfigurationJob` reads AudioSpecificConfig from an `mp4a` description,
  including QuickTime sound descriptions version 1 and 2 with `wave` extensions;
  `ObjectTypeScan` reads any `mp4a` or `mp4s` description's object type and
  decoder-specific information. Callers check the bytes and the audio object type.

## Limits and errors

- **Input.** The caller owns I/O through the core `InputCursor`. `BoxReader` retains
  only the boxes the caller asks for with `read_metadata`; `mdat` is never retained.
  A reader is bound to one input transaction: after a reset, build a new one.
- **Bounds.** `Limits` caps boxes, table records and sample descriptions (defaults
  1,000,000, 1,000,000 and 256), and each job has limits of its own. All storage is
  charged to the shared `Budget`; tables stay as compact wire runs, and nothing is
  stored per sample.
- **Clocks.** `Sample` carries exact source ticks, and `Sample::source_times` converts
  them to core `Time`. A fragment with no `tfdt` and no `FragmentContext::decode_origin`
  has unknown times, not zero. `CompositionOffset` keeps the version, the raw bits and
  the signed reading apart; an absent offset differs from an explicit zero.
- **Payload ranges.** `SampleReader::new` requires the sample to lie inside the
  caller's `mdat` extent, and `PhysicalSampleCursor::new_with_media_extents` checks
  every track's ranges against the extents before emitting anything; a valid but
  backward-ordered layout is `Unsupported` for the physical cursor only.
- **Edits.** Edit lists are parsed and mapped at rate one; other rates and dwell edits
  are refused. The source view never repeats or shifts samples.
- **Seek.** The planners select a preceding container sync sample, record available
  and missing preroll, and check every sample's byte range before publishing a plan.
  A sync sample is a container fact, not proof of decoder recovery; `roll` and `prol`
  groups are read where the caller asks. `sidx` and `tfra` entries are hints, not
  checked against the media.
- **Errors.** Unknown box versions and flags, `stz2` widths other than 4, 8 and 16,
  non-unit edits and inputs outside a compatibility policy are `Error::Unsupported`;
  structural damage is `Invalid` or `Truncated`; a passed limit is `Limit`. Failures
  are sticky: build a new reader or job to continue.
- **Not handled.** External data references, encryption, interpretation of `udta` and
  `meta`, decoder recovery and muxing.

## FFmpeg-compatible view

`compat::mov` reproduces the packet order, timestamps, flags, edits, seeks and
recovery of FFmpeg's MOV demuxer run with `+noparse+nofillin`
(`DemuxProfile::NoParseNoFillin`), for a consumer that chooses FFmpeg's result. It
reads the same parsed tables, pairs each packet with its untouched source sample and
never rewrites source facts. `packets::PacketCursor` walks the tracks in the demuxer's
order, `edits` applies edit lists as the demuxer does, including the trims of encoded
audio for range playback, `seek` builds the demuxer's index and
seeks one track or all of them, `front` reads a forward-only input, `encoded` admits
encoded sample entries, `recovery` keeps malformed tables readable as the demuxer does,
and `pcm` groups QuickTime PCM samples into the demuxer's packets. The module
documentation names the FFmpeg revision it reproduces.

## Comparison traces

Values that only make sense when comparing traces with FFmpeg, such as its return
codes, log lines and models of its input layer, are compiled only with the non-default
`reference-trace` feature, under `reference_trace::mov`. The comparison worker
`examples/isobmff_consumer_worker.rs` needs that feature: it drives the
FFmpeg-compatible open, index, seek and range-plan operations, and its
`--recover-regular-tables` profile reads a single local, unedited track through the
regular table recovery cursors. `cargo xtask codecs isobmff-reader` compares every
packet `Reader` delivers with the formats reference's `ffprobe`; its contract, rules and
reviewed differences are in [harness/features/isobmff-reader](../../harness/features/isobmff-reader).
The [PCM packet](../../harness/features/mov-pcm-packets/contract.json),
[table recovery](../../harness/features/mov-run-recovery/contract.json) and
[encoded view](../../harness/features/mov-encoded-views/contract.json) comparisons
check MOV packet fields and payloads against the demuxer. The
[QuickTime PCM sample](../../harness/features/quicktime-pcm-samples/contract.json)
comparison checks the concatenated PCM bytes and whole-frame counts per stream.
The [sound entry oracle](../../harness/features/quicktime-sound-description/contract.json)
checks the complete version 0 description and the supported PCM layout.
[DTS configuration](../../harness/features/isobmff-dts-signalling/contract.json)
compares the core-signalling verdict, coded sampling frequency and samples per access
unit with the demuxed configuration. Where an entry declares no access-unit clock,
the comparison uses its sample-entry rate. An
[independent entry oracle](../../harness/features/isobmff-dts-entry-source/contract.json)
checks the declared base rate and the exact access-unit clock separately.
The [one-track seek comparison](../../harness/features/mov-track-seek/contract.json)
checks demux index landings after opening and after draining to EOF, including the
first-key fallback. The
[source seek comparison](../../harness/features/isobmff-source-seek/contract.json)
checks sample ranges, zero-based source decode clocks and payloads with caller-specified
preroll, and compares composition-time selections with demuxed presentation timestamps.

The [segment index oracle](../../harness/features/isobmff-segment-indexes/contract.json)
checks the complete `sidx`, `tfra` and `mfro` fields and derived ranges.
The [index landing comparison](../../harness/features/isobmff-index-landings/contract.json)
checks backward seek ranges and first-packet fragment clocks against the demuxer.
The [MOV trace comparison](../../harness/features/mov-reference-trace/contract.json)
checks public packet clocks, open diagnostics and configuration, the first packet
read result, and open-time reads and seeks with fixed rewind geometry.

Comparison results and gates are in [docs/status.md](../../docs/status.md) and
[docs/quality.md](../../docs/quality.md).

## Specifications

- Apple QuickTime File Format; Adobe F4V 10.1 §§1.3, 2.4–2.16 (boxes, tables,
  fragments, `tfra`/`mfro`).
- ETSI TS 126 244 V18.0.0 §13.4 (`sidx`) and §13.5 (`tfdt`).
- ISO/IEC 14496-14:2020 §6.7 (sample entries and elementary stream descriptors).
- ISO/IEC 14496-12:2008/Amd1:2009 §§8.6.1.3.1–.3 (signed `ctts`);
  14496-12:2008/FDAmd2 §8.8.7.1 (moof-relative base).
- MPEG m76694 Table 9 and FDAM3 draft text for `stz2`, signed `trun` and
  `roll`/`prol`. These are draft sources: the code refuses fields they leave open.
- ETSI TS 102 366 Annex F (AC-3/E-AC-3 in ISO media).

The integration tests under `tests/` use independently authored fixtures in
`tests/fixtures/`, with expected values written from the specifications; hashes and
provenance are in `tests/fixtures/PROVENANCE.json`, and fixtures made with FFmpeg
document their commands (for example
[range-encoded-edits](tests/fixtures/range-encoded-edits/README.md)).
