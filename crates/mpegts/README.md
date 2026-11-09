# triergon-mpegts

Bounded, incremental MPEG-2 transport stream reader for 188-byte TS and
192-byte M2TS. It is a format-layer crate on top of `triergon-core` (input cursor,
budgets, exact time), `triergon-bitstream` (the section CRC) and
`triergon-framing` (access-unit framing). It frames packets, checks
continuity, assembles PSI sections and PAT/PMT tables, and assembles PES
packets with their raw timestamps. `Reader` composes those source facts with
the existing framers to implement `ContainerReader` over complete access units.
A PES is a byte run, never an access-unit packet. Decoding belongs to the codec
crates. The crate does no I/O and does not seek. The `mux` module writes a
single-program stream for one video elementary stream ([Writing TS](#writing-ts)).

The crate exposes two views:

- **Source view** (`Reader`, `TransportStream` and its parts). Every value is a wire
  fact: raw 33-bit PTS/DTS, raw PCR, descriptor bytes, source byte ranges.
  Unknown DTS stays unknown.
- **FFmpeg-compatible view** (`compat::pes`, `compat::input`). It reproduces
  the packets, positions, flags and timestamps of FFmpeg's MPEG-TS demuxer,
  including its handling of damaged and headerless PES. It is fed from the same
  transport packets and never rewrites source facts.

The non-default `reference-trace` feature adds `reference_trace`, the projections
that exist only to compare traces with FFmpeg: `recovery` (packets from rejected
carriers), `catalogue` (programs and streams), `packet_completion` (inputs without
media), `dvb` and `input`.

## Access units

`Reader::new(&pool, source, capabilities, ReaderLimits::default(), input_window)`
opens a sequential TS188 source. Set `ReaderLimits::format` for M2TS192, and
`program` to select a program explicitly. Without a selection, the PAT must
contain exactly one media program. `ContainerReader` supplies the normal
input-window, work, cancellation, backpressure and reset protocol. Seeking
returns `Error::Unsupported` without changing reader state.

The reader admits AVC and HEVC Annex B, single-block ADTS AAC-LC, LATM in LOAS envelopes, and
AC-3 or E-AC-3 with unambiguous DVB descriptors in private PES or stream
types `0x81` and `0x87` in the caller-declared ATSC namespace. Tracks retain
the original stream type as the codec tag. [ATSC A/53 Part 3, §5.7](https://www.atsc.org/wp-content/uploads/2023/02/A53-Part-3-2023-02.pdf)

An E-AC-3 packet contains the primary independent syncframe and up to eight
consecutive dependent syncframes with the same sample rate and block count.
`Ac3AccessUnit::syncframes()` exposes each header in source order, and the
primary supplies the packet's duration. DVB groups may cross PES boundaries.
The ATSC reader admits groups headed by a six-block independent syncframe
and requires the whole group in one PES. [ATSC A/52, Annexes E and G](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf)

LATM streams with type `0x11` use audio PES stream IDs `0xc0` through `0xdf`.
The default reader delimits complete byte-aligned LOAS envelopes across PES
boundaries and preserves their original byte ranges. The multiplex element is
opaque, so sample count, duration and decoder configuration are unspecified.
`compat::latm::PacketPolicy::PreservePes` exposes each validated, nonempty
LATM PES and its clock context as a separate `ReaderEvent::LatmPes` observation.
[ITU-T H.222.0, §2.11.2](https://www.itu.int/rec/dologin_pub.asp?id=T-REC-H.222.0-202308-S%21%21PDF-E&lang=e&type=items)

The framers preserve original parameter sets and coded audio envelopes.
MPEG audio/video, subtitles, descriptors declaring additional independent
E-AC-3 services and ambiguous private streams report `ReaderEvent::Unsupported`
with original PMTs and PES available as source events.

`Packet` implements `PacketInfo` and retains its exact PMT, source configuration epoch,
framer syntax and intersecting PES events. `sources()` maps its elementary-byte
extent to retained PES events, including all physical source and header ranges.
Framer coordinates use a private concatenated elementary view (`SourceId(0)`),
not physical file offsets. Video scatter payloads are coalesced cooperatively
into charged immutable storage. Audio packets borrow their access unit's stored
bytes. Cloning a packet or `SourceEvent` shares the charged storage.

`Track::configuration_epoch()` comes from its elementary route, which can be
reused across PMT versions and PCR binding changes. In-band SPS/PPS changes remain
in the source NALs and do not advance it. Consumers track parsed codec-configuration
changes separately.

`ReaderEvent::Source` delivers every original `TraceEvent` in order. PMT track
catalogues change atomically with the corresponding source event. A PMT, clock
or interrupted-byte-run boundary reports unpublished bytes through
`ReaderEvent::Interrupted`, rather than treating a loss boundary as EOF.
Logical EOF drains the framers before the original `Finished` event and final
`Step::End`. Audio defaults to `AudioEofPolicy::RejectIncomplete`: an unfinished
frame still returns `Truncated`. With `AudioEofPolicy::ReportIncomplete`, only
logical source EOF can instead produce `ReaderEvent::IncompleteAudio` carrying
`AccessUnit::IncompleteAudio`. It keeps the exact unfinished audio bytes,
truncation reason, validated PES ranges and any applicable one-use source anchor.
It has no complete-frame duration and is never a neutral packet or complete AU.
Copying is bounded by the audio frame limit, cooperative work and the shared
storage budget. Retained fragments survive reader reset or closure. Clock,
program and loss boundaries remain interruptions; transport failures and invalid
headers are not recovered by this reporting policy.
All framers finish before EOF fragments are reported. Their publication counts
the remaining reader tables and distinct backing allocations against the storage
limit; caller-held output and external reservations can still cause retryable
backpressure.

Video defaults to `compat::annex_b::VideoEofPolicy::RejectUnframed`. With
`VideoEofPolicy::ReportUnframed`, a final AVC or HEVC byte run with invalid
leading bytes and no Annex B start code produces `ReaderEvent::IncompleteVideo`
at logical source EOF. It retains the validated PES spans and first source
clock anchor. Its byte extent follows the video access-unit limit. The event
is separate from the neutral packet stream. An interruption ends the run,
and a later start code within a run preserves the leading-byte refusal.

Annex-B groups without an admitted primary/base-layer picture
remain `ReaderEvent::VideoHeaders`,
outside the AU packet stream. Packets and events remain usable after close or reset.

A completed PES whose header predates a program boundary remains available as
`ReaderEvent::InterruptedPes`, together with the boundary event. Framing resumes
with PES that start after the boundary. This keeps a retained PES from inheriting
a newer PMT or PCR binding.

A PES time anchor applies once, to the first AU whose start-code/sync byte is
at or after that PES start. An anchor inside a frame waits for the next frame.
`PacketClock` retains raw timestamps, the source clock key and lifting status.
Quarantined source clocks keep their raw fields and an explicit flag, while
their lifted timestamps remain unresolved.
The default policy explicitly chooses period zero; `Unanchored` and bounded
source-clock policies are also available. Missing DTS and later unanchored
per-frame timestamps stay absent. Exact audio sample duration is available
through `sample_duration()`; the neutral 90-kHz duration stays absent when it
cannot represent that duration exactly. Codec configuration NALs and audio
headers stay on the rich source types; the PMT track does not fabricate a
decoder configuration record.

`ReaderLimits` bounds PES retention per pending AU, framed output count per
byte run, audio frame size, video NAL/AU sizes and elementary input windows,
in addition to the transport limits and shared live-capacity budget.

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
| Descriptors | `psi::Descriptor::{registration, languages, avc_video, hevc_video, dvb_ac3, dvb_eac3, dvb_subtitling}` |
| Stream codec | `psi::ElementaryStream::{codec, dvb_descriptor_codec}` → `triergon_core::CodecId` |
| PES | `PesAssembler` → `Pes`, `PesHeader::{raw_pts_time, raw_dts_time}` |
| Timestamps | `clock::source_timestamp` with `clock::SourceTimePolicy` |
| FFmpeg-compatible PES | `compat::pes::DemuxObserver::{new_registered, new_private_audio, new_hls_private_audio, new_dvb_subtitles}`; `compat::pes::PesCutter` → `CutPes` for one PID of any stream type |
| FFmpeg-compatible streams | `compat::stream::{ProgramReader, codec, configuration, is_video}`: programs, stream codecs (adds 0x81, 0x87, 0x90 and DVB tags outside a DVB namespace) and subtitle configuration |
| FFmpeg-compatible clock | `compat::pes::DemuxClock::{project_header, place}`, `compat::pes::pcr_bounded_pts` (teletext PTS) |
| FFmpeg-compatible start position | `compat::input::InputBootstrap` |

The PMT track view borrows its one-byte `stream_type` tag. It reports the codec
identified by the stream type or, for private PES data in a DVB namespace, the
first descriptor that names a codec. Unidentified codecs retain their tag in
`CodecId::Other`. The descriptor bytes remain ordered source data.

`compat::pes::selected_track_kind` preserves the playback view. DVB subtitles and
teletext are classified from signalling. Other private PES entries require an
admitted observer for the active map. The private types 0x81 and 0x87 indicate
audio, and 0x90 indicates presentation graphics in this view. MPEG-1 video
(0x01) keeps its source kind and is unknown in the selected PES view.

The sequential feed keeps program tables, clock transitions and diagnostics.
`TransportStream::poll_feed` can deliver validated PES continuations directly
in both feed modes. `poll_feed_carriers` lets a progressive feed move a
plain continuation's charged payload storage into `compat::pes::DemuxObserver`.
`poll_feed_with_payload_runs` can offer several payload-only continuations
on one PID from an already shared window. Each carrier keeps its source
range and event ordinal. Runs reserve checked span capacity before advancing
input; ordinary carrier polling handles PES completion.
Feed dispatch reuses the selected PES route after checking its PID against the
current program table. Other PIDs use the ordinary route lookup.
Idle PSI jobs do not enter the table poller. Complete timestamp-only PES headers
use the shared timestamp validator directly; other headers retain incremental
validation. Both paths preserve work charges and cancellation boundaries.
Runs record the complete final carrier for duplicate detection before transferring
the shared backing storage. Every retained view keeps the full allocation charged.
A fresh, inactive `DemuxObserver` can select `enable_compact_payloads` before input and then use
`poll_compact_payload` (or its input-bound counterpart). `CompactDemuxPacket`
retains one backing handle per admitted run on 64-bit targets; `CompactPayload::spans` and its
consuming `PayloadCursor` expose every logical carrier's payload range, source
range, transport extent and context. Span capacity remains charged for the full
logical count. Retiring the last range of a run releases its backing handle.
On 32-bit targets the same cursor uses ordinary span storage, keeping the
observer's charged size and constant-time counters unchanged.
Ordinary constructors and polling keep the owned `DemuxPacket.spans` API.
Mixing polling representations returns a typed error before changing state or
spending work. `CompactPayload::from_spans` always adopts ordinary spans and
never interprets public metadata as a run marker.
`PayloadCursor::current_bytes` and `next_bytes` expose just the borrowed byte
range with the same admission and retirement rules. Full carrier facts remain
available through `current` and `next_span`.
Sequential playback can share up to four tight input windows of at most 4 KiB
each. A full set, oversized backing storage or low budget headroom uses
carrier-sized storage. Fallback uses the original transport event.
`take_feed_omissions` counts
skipped events so consumers can retain positions in the source trace.

`TransportStream::poll_compact` coalesces runs of null packets into one
`CompactTraceEvent::NullRun`.

### Writing TS

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
  `TransportStream` does not read this output back: its source view accepts
  unbounded PES only on video streams (0x01, 0x02, 0x1b, 0x24).
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

`tests/mux.rs` checks SDT size limits and CRC, name encoding, PID and program
validation. The written stream is compared with FFmpeg's through the capture
pipeline ([triergon-capture](../capture/README.md#conformance)).

## Supported

| Area | Coverage |
| --- | --- |
| Packets | TS188, M2TS192 (4-byte prefix kept opaque), adaptation field, PCR/OPCR, private data, extension bytes kept raw |
| Continuity | per-PID counters, wrap, one duplicate packet (with the PCR exception), loss, signalled discontinuity |
| PSI | pointer field, multi-section packets, CRC-32, PAT, PMT, program and ES descriptor loops |
| Descriptors | registration, ISO 639 language, AVC, HEVC, DVB AC-3 (`0x6a`), E-AC-3 (`0x7a`), DVB subtitling (`0x59`), teletext (`0x56`) |
| PES | split headers, finite lengths, unbounded lengths on MPEG-1/2 video (0x01, 0x02), AVC (0x1b) and HEVC (0x24) streams, PTS/DTS with marker checks, optional fields |
| FFmpeg-compatible carriage | AVC, HEVC, AAC ADTS, AAC LATM (`RegisteredCarriage`); AC-3/E-AC-3 by DVB tag or stream type `0x81`/`0x87` (`PrivateAudioCarriage`); single-service DVB subtitles; PES packets of any stream (`PesCutter`), including PGS (`0x90`), teletext and caption-carrying video |

## Behaviour and limits

- **Input.** The caller owns I/O through the core `InputCursor` and polls with
  a `WorkBudget` and a cancel flag. One poll yields at most one event. The
  packet size is chosen by the caller; there is no resynchronisation or
  auto-detection in the source view.
- **Bounds.** `ParseLimits` caps section bytes (default 4096, maximum 4096),
  PES bytes (16 MiB), source spans (4096), programs (64), streams (64) and
  descriptors (256). All storage is charged to the shared `Budget`; emitted
  `Bytes` charge their whole backing allocation. A rejected push returns the
  original bytes; backpressure must be resolved by the caller releasing
  output, not by retrying in a loop.
- **Program selection.** `TransportStream` produces PES for one program: the
  one named by the caller, or the only program in the PAT
  (`new_single_program_segment`). Other PAT programs are reported but not
  demultiplexed. A PAT/PMT becomes active only when complete, current and
  CRC-valid.
- **Damage.** `TransportStream` reports each fault as
  `TraceEvent::Diagnostic` and keeps going. TEI, scrambled, lost and
  excess-duplicate packets discard the affected partial section or PES; their
  payload is never used. A bad PSI pointer is reported and parsing resumes at
  the next payload unit start. The standalone assemblers keep a fault sticky
  until their `discard`.
- **Timestamps.** `PesHeader` keeps raw 33-bit PTS/DTS; a missing DTS is never
  filled from PTS in the source view. `clock::source_timestamp` lifts raw
  values to exact time only under an explicit `SourceTimePolicy`
  (`Unanchored`, `RelativeEpochZero`, or `Bounded` by a PCR/PTS anchor) and
  reports ambiguous or out-of-bound values instead of guessing a wrap.
  Invalid PCR signalling sets `clock_is_quarantined()` until an explicit
  `discontinuity()`.
- **Discontinuities and segments.** A signalled PCR discontinuity followed
  by a new PCR starts a new clock epoch (`TraceEvent::Timeline`) and drops
  partial PES. A caller `discontinuity()` (for example an HLS discontinuity
  tag between segments) advances both clock and configuration epochs and
  resets PSI state. Ending one source entity (`media_entity_end`) does not
  flush a PES; only `end_of_source()` at logical end of media does.
- **Descriptor namespace.** Private descriptor tags are interpreted as DVB
  only under `psi::DescriptorNamespace::Dvb`, supplied by the caller through
  `new_with_namespace`. The default is `Unspecified`.
- **FFmpeg-compatible view.** Each observer is bound to one program, PID,
  stream type and stream ID (`DemuxScope`). Contexts outside the observed
  profile return `Error::Unsupported`.
- **Not handled.** SI tables beyond PAT/PMT (their packets are still reported),
  ATSC/Blu-ray signalling, descrambling, seeking, multi-program PES output.

## Conformance

Specification basis:

- ITU-T H.222.0 (08/2023): §2.4.3.1–2.4.3.5 (transport packet, adaptation
  field), §2.4.3.6–2.4.3.8 (PES), §2.4.4 and §2.6 (PSI and descriptors),
  Annex A (CRC-32).
- ETSI EN 300 468: §6.2.41 / Table 99 (subtitling descriptor), Annex D.3 and
  D.5 (AC-3 and E-AC-3 descriptors).

Verification:

- Integration tests under `tests/` use independently authored transport
  streams in `tests/data/` (listed in `tests/data/manifest.json`) with
  expected values derived from the specifications.
- The source view is compared field by field with an independent oracle over
  the corpus. The FFmpeg-compatible view is compared packet by packet
  (payload bytes, timestamps, flags, positions, stream-ID side data) with the
  FFmpeg reference; nonmatching cases are kept as failures, not normalised.
  `cargo xtask report` runs that comparison from
  [`harness/features/mpegts-pes/`](../../harness/features/mpegts-pes/contract.json)
  through `playback-worker`'s `ts-` modes, which use the public API only:
  `TransportStream` selects the program, its first active PMT binds a
  `compat::pes::DemuxObserver` to the one elementary stream, and `DemuxClock`
  places the timestamps. The 272 recorded requests are 97 compat-view
  operations, of which 66 match. The 168 requests for the strict source view, 64
  of them for its native clock, are replayed as the compat-view operation on
  their inputs: the strict source view and its native clock are not compared.
  The 31 operations that do not match are open cases that need a program-level
  reader (the MPEG-TS `ContainerReader`): PAT/PMT routing, clock and format
  boundaries, recovery of rejected and damaged carriers, input across media
  entities, or a program-level outcome where neither side emits a packet. They
  are `pcr-timeline-discontinuity`, `pcr-extension-invalid`, `transport-tei`,
  `transport-scrambled`, `transport-afc-reserved`,
  `transport-adaptation-overrun`, `transport-partial-final-packet`,
  `transport-sync-loss`, `psi-crc-invalid`, `psi-pointer-overrun`,
  `psi-length-over-policy`, `psi-pending-not-current`,
  `pat-atomic-two-sections`, `pat-incomplete-version`,
  `pat-network-pid-not-track`, `psi-pointer-finishes-old-starts-new`,
  `psi-two-sections-one-payload`, `pmt-version-wrap-retypes-pid`,
  `pmt-opaque-language-descriptors`, `pmt-descriptor-overrun`,
  `pmt-multi-section-forbidden`, `psi-stuffing-tail-invalid`,
  `psi-truncated-eof`, `pes-timestamp-marker`, `pes-forbidden-time-flags`,
  `pes-optional-header-overrun`, `pes-start-prefix-invalid`,
  `hls-contiguous-pes-carry`, `hls-external-map`, `hls-explicit-discontinuity`,
  `opcr-without-pcr-invalid`. On the 66, the worst runtime bound is 1.0029 of
  the reference, peak heap at most 0.073 and total resident RSS at most 0.818 of
  it.

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
