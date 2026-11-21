# triergon-mpegts

Bounded, incremental MPEG-2 transport stream reader for 188-byte TS and
192-byte M2TS. It is a format-layer crate on top of `triergon-core` (input cursor,
budgets, exact time), `triergon-bitstream` (the section CRC) and
`triergon-framing` (access-unit framing). It frames packets, checks
continuity, assembles PSI sections and PAT/PMT tables, and assembles PES
packets with their raw timestamps. `Reader` composes those source facts with
the existing framers to implement `ContainerReader` over complete access units.
A PES is a byte run, never an access-unit packet. Decoding belongs to the codec
crates. The crate does no I/O. The `mux` module writes a
single-program stream for one video elementary stream ([Writing TS](#writing-ts)).

The crate exposes two views:

- **Source view** (`Reader`, `TransportStream` and its parts). Every value is a wire
  fact: raw 33-bit PTS/DTS, raw PCR, descriptor bytes, source byte ranges.
  Unknown DTS stays unknown.
- **FFmpeg-compatible view** (`compat::pes`, `compat::stream`, `compat::input` and
  the opt-in policies in `ReaderLimits`). It reproduces the packets, positions,
  flags and timestamps of FFmpeg's MPEG-TS demuxer, including its handling of
  damaged and headerless PES. It is fed from the same transport packets and never
  rewrites source facts.

The non-default `reference-trace` feature adds `reference_trace`: comparison-only
projections (`recovery`, `catalogue`, `packet_completion`, `dvb`, `input`).

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{PacketFormat, ParseLimits, TraceEvent, TransportStream};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../playback/tests/fixtures/hls/media-00.ts");
let stream = std::fs::read(path)?; // any transport; the parser does no I/O
let pool = Budget::new(64 << 20)?;
let caps = SourceCapabilities { length: Some(stream.len() as u64), random_access: false };
let mut input = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
let program = 1;
let mut ts = TransportStream::new(pool.clone(), PacketFormat::Ts188, ParseLimits::default(), program)?;
let cancel = AtomicBool::new(false);
let mut pes_count = 0;
loop {
    match ts.poll(&mut input, &mut WorkBudget::new(1 << 16), &cancel)? {
        Step::Ready(TraceEvent::ProgramMap { map, active: true, .. }) => { /* map.streams */ }
        Step::Ready(TraceEvent::Pes(pes)) => {
            // pes.pid, pes.header.pts_raw33, pes.header.dts_raw33, pes.payload()
            pes_count += 1;
        }
        Step::Ready(TraceEvent::Diagnostic(fault)) => { /* kind, pid, location, error */ }
        Step::Ready(TraceEvent::Finished) => break,
        Step::Ready(_) => {} // transport packets, sections, PAT, timeline
        Step::NeedInput(req) => match stream.get(usize::try_from(req.offset)?..) {
            Some(rest) if !rest.is_empty() => {
                let bytes = Bytes::copy_from_slice(&pool, &rest[..rest.len().min(req.maximum)])?;
                input.provide_owned(InputWindow::new(req.source, req.token, req.offset, bytes)?)?;
            }
            _ => input.finish(req.source, req.token)?,
        },
        Step::Yield => {}
        Step::End(_) => ts.end_of_source()?, // only at logical end of media
    }
}
assert!(pes_count > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`examples/mpegts_packets.rs FILE [PROGRAM]` prints the streams and PES packets of a
file.

| Task | Entry points |
| --- | --- |
| Complete source access units | `Reader`, `ReaderLimits`, `ContainerReader` |
| Whole stream, one program | `TransportStream::{new, new_with_namespace}` |
| One HLS segment (TS188, in-band PAT/PMT) | `TransportStream::new_single_program_segment`, `new_single_program_feed` |
| Sequential TS188 playback input | `TransportStream::new_single_program_progressive_feed` |
| Packets only | `parse_transport`, `TransportFramer`, `carrier::TransportInspector` |
| Continuity | `Continuity::observe` → `ContinuityOutcome` |
| PSI | `SectionAssembler`, `psi::{PatReader, PmtReader, PatCollector}` |
| Track classification | `psi::ElementaryStream` implements `triergon_core::container::TrackInfo` |
| Descriptors | `psi::Descriptor::{registration, languages, avc_video, hevc_video, dvb_ac3, dvb_eac3, dvb_subtitling, dvb_dts_hd}` |
| Stream codec | `psi::ElementaryStream::{codec, dvb_descriptor_codec}` → `triergon_core::CodecId` |
| PES | `PesAssembler` → `Pes`, `PesHeader::{raw_pts_time, raw_dts_time}` |
| Timestamps | `clock::source_timestamp` with `clock::SourceTimePolicy` |
| FFmpeg-compatible PES | `compat::pes::DemuxObserver::{new_registered, new_private_audio, new_hls_private_audio, new_dvb_subtitles}`; `compat::pes::PesCutter` → `CutPes` for one PID of any stream type |
| FFmpeg-compatible streams | `compat::stream::{ProgramReader, codec, configuration, is_video}`: programs, stream codecs (adds 0x81, 0x87, 0x90 and DVB tags outside a DVB namespace) and subtitle configuration |
| FFmpeg-compatible clock | `compat::pes::DemuxClock::{project_header, place}`, `compat::pes::pcr_bounded_pts` (teletext PTS) |
| FFmpeg-compatible start position | `compat::input::InputBootstrap` |

A PMT track reports the codec named by its stream type or, for private PES data in
a DVB namespace, by the first descriptor that names one; others stay
`CodecId::Other` with their tag. `poll_compact` coalesces null-packet runs; the feed
polls (`poll_feed`, `poll_feed_carriers`, `poll_feed_with_payload_runs`) hand PES
continuations directly to a `compat::pes::DemuxObserver`.

## Access units

`Reader::new(&pool, source, capabilities, ReaderLimits::default(), input_window)`
opens a sequential TS188 source. Set `ReaderLimits::format` for M2TS192, and
`program` to select a program explicitly. Without a selection, the PAT must
contain exactly one media program. `ContainerReader` supplies the normal
input-window, work, cancellation, backpressure and reset protocol.
`Reader::new_with_packet_tracks` restricts framing and packet events to chosen
elementary PIDs; track metadata, PES and transport diagnostics remain visible for
every stream.

| Carriage | Signalling | Access unit |
| --- | --- | --- |
| AVC, HEVC | stream types `0x1b`, `0x24` | Annex B access unit |
| MPEG-1/2 audio Layer I–III | stream types `0x03`, `0x04`, PES IDs `0xc0`–`0xdf` | `MpegAudioFrame` |
| AAC-LC in ADTS | stream type `0x0f` | single-block ADTS frame |
| LATM in LOAS | stream type `0x11`, PES IDs `0xc0`–`0xdf` | `LoasFrame` (multiplex element opaque) |
| AC-3, E-AC-3 (DVB) | `0x06` with descriptor `0x6a` / `0x7a` | `Ac3AccessUnit` |
| AC-3, E-AC-3 (ATSC) | `0x81` / `0x87` under `DescriptorNamespace::Atsc`, PES ID `0xbd` | `Ac3AccessUnit` |
| DTS core | `0x06` with DVB descriptor `0x7b`, or `ReaderLimits::unlabelled_dts_core` | `DtsAudioFrame` |
| DTS-HD | `0x06` with DVB DTS-HD descriptor | `DtsAudioFrame` or `DtsExtensionFrame` |
| DVB subtitles | `0x06` with descriptor `0x59`, PES ID `0xbd` | `AccessUnit::DvbSubtitle` |

- **MPEG audio** frames may span PES; free-format frames are confirmed by two
  consecutive frame spacings.
- **E-AC-3** (DVB) groups one independent substream with up to eight dependent
  syncframes, at most 36,864 bytes by default; ATSC needs a six-block independent
  syncframe with its dependents in one PES.
- **DTS** frames normal core frames, a core-bearing DTS-HD unit per aligned finite
  PES, or one to four extension substreams without a core, checking substream
  order, extents and header CRC; asset payloads stay opaque.
- **DVB subtitles**: one finite, aligned PES with PTS is one unit; the wrapper and
  end marker are validated, segments are not decoded.

Other streams emit `ReaderEvent::Unsupported`; their PMTs and PES remain source events.

Set `ReaderLimits::probe_unlabelled_dts_core` to admit one descriptor-free
private-data route in a program with empty program descriptors. The first
`private_stream_1` PES must start with two complete canonical big-endian DTS
core frames whose sample-rate, channel-mode and block-count fields agree.
The probe charges bounded work, preserves the original table and PES facts
and applies again at framing resets. A failed initial probe reports the
route as unsupported and later PES cannot rescue it.

LCEVC video and linkage descriptors have bounded typed readers. They retain
the original descriptor bytes and validate profile, reserved and linkage fields.

### Packets, events and timing

`Packet` implements `PacketInfo` and retains its PMT, source configuration epoch,
framer syntax and the PES events it intersects (`sources()`, with all physical
source and header ranges). Cloning a packet shares its charged storage.
`Packet::sample_duration` gives the exact audio sample span; the neutral 90 kHz
duration stays absent when it cannot represent that span exactly.

A PES time anchor applies once, to the first access unit whose start code or sync
byte is at or after that PES start. `PacketClock` retains raw timestamps, the
source clock key and lifting status. The default `time_policy` lifts timestamps in
period zero (`RelativeEpochZero`); `Unanchored` and bounded policies are available.
Missing DTS and unanchored per-frame timestamps stay absent; quarantined source
clocks keep their raw fields with lifted values unresolved.

Source events arrive in order: `ReaderEvent::Transport` (each complete carrier),
`Continuity`, `Source` (other trace events) and track catalogue changes with the
PMT event that publishes them. Other events a caller handles:

| Event | Meaning |
| --- | --- |
| `Interrupted` | a table, clock or byte-run boundary cut unpublished elementary bytes |
| `InterruptedPes` | a completed PES whose header predates a program boundary |
| `VideoHeaders` | an Annex B group without an admitted picture |
| `Unsupported` | no framer for this stream |
| `IncompleteAudio`, `IncompleteVideo`, `DvbSubtitlePrefix`, `UnframedAudio` | EOF reports, only under the opt-in policies below |
| `AdtsFragment`, `RecoveredAdts`, `LatmPes` | FFmpeg-compatible packet boundaries, only under the opt-in policies below |

Logical EOF drains the framers before the original `Finished` event and the final
`Step::End`. Packets and events remain usable after close or reset.

`Reader::step_with_carriers` additionally passes each transport and continuity
observation to a borrowed callback in source order. `TransportObservation::to_packet`
retains a carrier.

### Entities

For an ordered stream (for example HLS segments), call `Reader::finish_entity`
with the final input token and the next entity's source ID and capabilities.
`EntityTransition::Contiguous` carries tables, continuity, partial PES and
access-unit state forward; `Discontinuity` reports unfinished access units and
waits for the next PAT and PMT. Source ranges keep each entity's physical
coordinates. Call `finish` on the last entity.

### Seeking

`Reader::request_seek` scans one seekable, length-known entity for the last
eligible access unit with the greatest presentation time at or before the target:
an AVC or HEVC random-access picture with resolved timing; a timestamped AAC-LC
frame in ADTS or LOAS (with a complete in-band configuration), AC-3, E-AC-3 or MPEG
audio frame (Layer III with an empty bit reservoir); a DTS core frame needing no
decoder history; or a DTS-HD XLL entry (construction 17, or 14 with a separate
core) with one extension-zero asset, its own static setup, sync at offset zero, no
decoding delay or smoothing buffer and a valid common-header CRC.

An `Event::Seek` precedes the landing and carries the discard time for
`PlaybackPreroll`; dependent frames follow as preroll with their original bytes,
clocks and source ranges. The caller supplies decoder setup. The scan is bounded by
`ReaderLimits::maximum_seek_scan_bytes` (64 MiB by default; zero disables seeking).
Seeks across selected-track source epochs and ambiguous clock wraps are refused.

| Method | Adds |
| --- | --- |
| `request_seek_in_epoch` | land only within one clock/configuration pair from `Packet::epochs()` |
| `request_seek_in_entities` | an ordered `SeekEntity` plan of contiguous entities; the scan bound covers their total |
| `request_seek_and_index` | a bounded landing index for later seeks to the same track |
| `request_seek_in_epoch_and_index` | the same, keyed by the epoch pair |
| `request_seek_in_entities_in_epoch`, `request_seek_in_entities_in_epoch_and_index` | both |

Explicit-epoch seeks replay from the start, publishing the original tables, clock
transitions and selected-epoch `VideoHeaders` before the landing. An index is reused
only for the same track, epoch pair and ordered source IDs and lengths; otherwise,
or when its limits are exhausted, the reader scans. Reset or close discards it.
Sources must remain stable between requests. For one clear entity with a single
Layer I/II, AVC or HEVC stream, the index can also hold restart checkpoints at
in-band PAT/PMT carriers; a later seek compares those carriers byte for byte and
reads no earlier bytes. Other landings replay the entity prefix.

### Opt-in policies

| `ReaderLimits` field | Non-default choice |
| --- | --- |
| `audio_eof_policy` | `ReportIncomplete`: unfinished audio at logical EOF becomes `IncompleteAudio` instead of `Truncated` |
| `video_eof_policy` | `ReportUnframed`: a final AVC/HEVC run without an Annex B start code becomes `IncompleteVideo` |
| `dvb_subtitle_tail_policy` | `ReportCompletePrefix`: a PES with an unfinished final segment yields its complete segments as `DvbSubtitlePrefix` |
| `unframed_audio_policy` | `ReportAtEof`: one opaque private-audio PES without a syncword becomes `UnframedAudio` |
| `adts_packet_policy` | `ReportFragments`: FFmpeg's parser framing, with opaque bytes as `AdtsFragment` |
| `adts_pes_policy` | `ReportUnbounded`: assemble zero-length ADTS PES (a H.222.0 §2.4.3.7 violation) |
| `latm_packet_policy` | `PreservePes`: one `LatmPes` per validated PES |
| `pcr_policy` | `IgnoreReservedBits`: accept PCRs with incorrect reserved bits |
| `descriptor_namespace` | `Dvb` or `Atsc`: interpret private descriptors and stream types |

## FFmpeg-compatible view

`compat::pes::DemuxObserver` cuts the transport packets of one selected PID
(`DemuxScope`: program, PID, stream type and stream ID) into the packets FFmpeg's
demuxer emits, beside the strict source view. A `DemuxPacket` may be partial and
says why it ended. Carriages: `RegisteredCarriage` (AVC, HEVC, AAC ADTS and LATM),
`PrivateAudioCarriage` (AC-3, E-AC-3) and `DvbSubtitlesDescriptors`. `PesCutter`
cuts any stream type, including PGS (`0x90`), teletext and caption-carrying video.
`DemuxClock` places timestamps in the demuxer's 33-bit interval beside the raw
values. Contexts outside the observed profile return `Error::Unsupported`.

Opt-in fault handling, each requiring the actual source diagnostic before output
and never reconstructing source clocks or validated PES:

- `TransportFaultRecovery` (with `PesFaultPolicy`) extracts complete finite AVC PES
  from TS188 carriers with one TEI or scrambling fault, isolated timing-header
  faults, checked PCR/OPCR rejections, a signalled PCR discontinuity
  (`confirm_source_timeline`) or a final partial carrier (`TransportTailConfirmation`).
- `MalformedInputCompletion` reports AVC carriers with reserved adaptation control,
  an adaptation overrun, an invalid PES start code or an overlong PES header.
- `PacketCompletion` confirms that an input held only control traffic
  (`NoMediaConfirmation`).
- `compat::annex_b::ContinuityReader` and `compat::opaque_video::OpaqueRecovery`
  report unframed video runs through continuity changes or a replayed fault.

## Writing TS

`mux::Muxer` writes 188-byte packets for one video elementary stream, one PES per
picture:

```rust
use triergon_mpegts::mux::{MuxConfig, Muxer};

let picture: &[u8] = &[0, 0, 1, 0xb3, 1, 2, 3]; // one coded picture
let mut muxer = Muxer::new(&MuxConfig::default())?;
let mut out = Vec::new();
muxer.write_picture(picture, 3000, 0, true, &mut out); // 90 kHz pts, dts, key; appends to `out`
assert_eq!(out.len() % 188, 0);
# Ok::<(), triergon_core::Error>(())
```

- Tables: PAT, PMT and a DVB SDT with one service descriptor (ETSI EN 300 468),
  version 0, serialised once at `Muxer::new`. The SDT is repeated every 500 ms and
  PAT/PMT every 100 ms of PCR time; PAT/PMT also precede a key picture that follows
  a non-key picture.
- PES: stream_id 0xE0, unbounded length, PTS and DTS. The first packet's
  adaptation field carries PCR = DTS·300 and, on key pictures, the
  random_access_indicator. Stuffing goes into the final packet's adaptation field.
  `TransportStream` reads the output back: the source view accepts unbounded PES
  on video stream types (0x01, 0x02, 0x1b, 0x24).
- `MuxConfig::default()`: transport_stream_id 1, original_network_id 0xFF01,
  program 1, PMT PID 0x1000, video PID 0x0100, stream type 0x01, provider
  "Triergon", service "Service01".
- `Muxer::new` returns `Error::Unsupported` for names that are not printable ASCII
  or do not fit one SDT packet together (158 bytes), program number 0, a PMT or
  video PID outside 0x0020–0x1FFE, equal PMT and video PIDs, and stream types other
  than 0x01 and 0x02.

| Item | FFmpeg 9.0.2 | `mux` | Reason |
|---|---|---|---|
| PMT stream type | 0x02 | 0x01 by default, 0x02 selectable | H.222.0 Table 2-34 reserves 0x02 for constrained-parameter 11172-2 video |
| SDT provider | "FFmpeg" | "Triergon" by default, selectable | do not claim another implementation's identity |

## Limits

`ParseLimits` (transport) defaults: section bytes 4096 (3–4096), PES bytes 16 MiB,
source spans 4096, programs 64, streams 64, descriptors 256.

`ReaderLimits` adds: audio syncframe 8192 bytes, DTS-HD PES 65,535 bytes, video NAL
4 MiB and access unit 8 MiB (1024 NALs), 4096 PES per access unit, 32,000,000
access units, a 64 KiB elementary input window and the 64 MiB seek scan.

All storage is charged to one shared `Budget`; an emitted `Bytes` charges its
whole backing allocation.

## Errors and recovery

- **Damage.** `TransportStream` reports each fault as `TraceEvent::Diagnostic` and
  keeps going. TEI, scrambled, lost and excess-duplicate packets discard the
  affected partial section or PES. A bad PSI pointer resumes at the next payload
  unit start. The standalone assemblers keep a fault sticky until `discard`.
- **Backpressure and limits.** A rejected push returns the original bytes.
  `Backpressure` means caller-held output must be released; retry after releasing,
  not in a loop. A total that cannot fit the pool returns `Limit`.
- **Cancellation.** Cancellation and backpressure preserve pending input.
- **Clocks.** Invalid PCR signalling sets `clock_is_quarantined()` until an
  explicit `discontinuity()`. A signalled PCR discontinuity with a new PCR starts a
  new clock epoch (`TraceEvent::Timeline`) and drops partial PES.
- **Entities.** `media_entity_end` does not flush a PES; only `end_of_source()` at
  logical end of media does. A caller `discontinuity()` (for example an HLS
  discontinuity tag) advances clock and configuration epochs and resets PSI.
- **Truncation.** Unfinished audio at EOF and an overlong final DVB subtitle
  segment return `Truncated` unless the matching policy is selected.

## Not supported

- Resynchronisation and packet-size auto-detection in the source view.
- PES output for more than one program; SI tables beyond PAT/PMT (packets are
  still reported); descrambling; Blu-ray signalling; other ATSC signalling.
- MPEG-1/2 video access units, MPEG-2.5 audio, shorter ATSC E-AC-3 independent
  syncframes, other DTS-HD seek layouts.

## Standards

- ITU-T H.222.0 (08/2023): §2.4.3 (transport packet, adaptation field, PES), §2.4.4
  and §2.6 (PSI and descriptors), Annex A (CRC-32); epochs per §2.4.3.5, §2.4.4.9–10.
- ETSI EN 300 468 §6.2.41, Annexes D and G; ETSI TS 101 154; ETSI EN 300 743 §§6.2,
  7.2; ATSC A/53 Part 3 §5.5.2, §5.7; A/52 Annex E §2.3.1, Annex G §3.3.
- ETSI TS 102 114 V1.6.1 (DTS, DTS-HD; seek recovery per §7.5.3.4, §8.3.1, §8.5 and
  Annex G.3.1); ISO/IEC 14496-3 §1.7.2 (LOAS); RFC 8216 (entity boundaries).

The [original-input controls](../../harness/fate/README.md) check source
program discovery events, PMT replacement, encoded clocks, PES payload ranges
and diagnostics for unaligned input.

## Verification

Integration tests use independently authored streams in `tests/data/` (listed in
`tests/data/manifest.json`). The DTS seek inputs are authored with
`cargo xtask fixture transport-dts-seek --record REFERENCE`. The comparison
contracts are `harness/features/mpegts-*/contract.json`; results are in
[docs/status.md](../../docs/status.md).
