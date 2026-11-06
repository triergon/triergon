# triergon-mpegts

Bounded, incremental MPEG-2 transport stream reader for 188-byte TS and
192-byte M2TS. It is a format-layer crate whose only dependency is
`triergon-core` (input cursor, budgets, exact time). It frames packets, checks
continuity, assembles PSI sections and PAT/PMT tables, and assembles PES
packets with their raw timestamps. A PES is a byte run, not a codec access
unit: access-unit framing belongs to `triergon-framing`, decoding to the codec
crates. The crate does no I/O and does not seek.

The crate exposes two views:

- **Source view** (`TransportStream` and its parts). Every value is a wire
  fact: raw 33-bit PTS/DTS, raw PCR, descriptor bytes, source byte ranges.
  Unknown DTS stays unknown.
- **FFmpeg-compatible view** (`Native*` types in `native_pes`,
  `native_recovery`, `native_input`, `native_catalogue`,
  `native_packet_completion`, and `clock::NativePesClockV1`). It reproduces
  the packets, positions, flags and timestamps of the pinned FFmpeg MPEG-TS
  demuxer (`clock::NativeClockProfile::ArchivedPesV1`), including its
  handling of damaged and headerless PES. It is fed from the same transport
  packets and never rewrites source facts.

## Usage

```rust
use triergon_mpegts::{PacketFormat, ParseLimits, TraceEvent, TransportStream};

let mut ts = TransportStream::new(budget.clone(), PacketFormat::Ts188, ParseLimits::default(), program)?;
loop {
    match ts.poll(&mut input, &mut work, &cancel)? {
        Step::Ready(TraceEvent::ProgramMap { map, active: true, .. }) => { /* streams */ }
        Step::Ready(TraceEvent::Pes(pes)) => { /* pes.header, pes.payload() */ }
        Step::Ready(TraceEvent::Diagnostic(fault)) => { /* kind, pid, location, error */ }
        Step::Ready(TraceEvent::Finished) => break,
        Step::NeedInput(request) => { /* satisfy through the InputCursor */ }
        Step::End(_) => ts.end_of_source()?, // only at logical end of media
        _ => {}
    }
}
```

| Task | Entry points |
| --- | --- |
| Whole stream, one program | `TransportStream::{new, new_with_namespace}` |
| One HLS segment (TS188, in-band PAT/PMT) | `TransportStream::new_single_program_segment`, `new_single_program_feed` |
| Packets only | `parse_transport`, `TransportFramer`, `carrier::TransportInspector` |
| Continuity | `Continuity::observe` → `ContinuityOutcome` |
| PSI | `SectionAssembler`, `psi::{PatReader, PmtReader, PatCollector}` |
| Descriptors | `psi::Descriptor::{registration, languages, avc_video, hevc_video, dvb_ac3, dvb_eac3, dvb_subtitling}` |
| PES | `PesAssembler` → `Pes`, `PesHeader::{raw_pts_time, raw_dts_time}` |
| Timestamps | `clock::source_timestamp` with `clock::SourceTimePolicy` |
| FFmpeg-compatible PES | `native_pes::NativePesObserver::{new_registered, new_private_audio_v1, new_hls_private_audio_v1, new_dvb_subtitles_v1}` |
| FFmpeg-compatible damaged input | `native_recovery::NativePesRecoveryObserver` |
| FFmpeg-compatible start position | `native_input::NativeInputBootstrap` |

`TransportStream::poll_feed` can deliver validated PES continuations directly
to a consumer in segment feed mode. `take_feed_omissions` counts the skipped
events so consumers can retain positions in the source trace.

`TransportStream::poll_compact` coalesces runs of null packets into one
`CompactTraceEvent::NullRun`. `examples/mpegts_source_trace.rs` prints the
source view as JSON lines (`mpegts_source_trace 188|192 PROGRAM [--dvb]
[--native-clock-v1] [--media-entities-v1] [--compact] FILE...
[--discontinuity FILE...]`); `examples/mpegts_native_pes_trace.rs` adds the
FFmpeg-compatible PES rows.

## Supported

| Area | Coverage |
| --- | --- |
| Packets | TS188, M2TS192 (4-byte prefix kept opaque), adaptation field, PCR/OPCR, private data, extension bytes kept raw |
| Continuity | per-PID counters, wrap, one duplicate packet (with the PCR exception), loss, signalled discontinuity |
| PSI | pointer field, multi-section packets, CRC-32, PAT, PMT, program and ES descriptor loops |
| Descriptors | registration, ISO 639 language, AVC, HEVC, DVB AC-3 (`0x6a`), E-AC-3 (`0x7a`), DVB subtitling (`0x59`) |
| PES | split headers, finite and unbounded (video) lengths, PTS/DTS with marker checks, optional fields |
| FFmpeg-compatible carriage | AVC, HEVC, AAC ADTS, AAC LATM (`NativeRegisteredCarriage`); AC-3/E-AC-3 by DVB tag or stream type `0x81`/`0x87` (`NativePrivateAudioCarriage`); single-service DVB subtitles |

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
  resets PSI state. Ending one source entity (`media_entity_end_v1`) does not
  flush a PES; only `end_of_source()` at logical end of media does.
- **Descriptor namespace.** Private descriptor tags are interpreted as DVB
  only under `psi::DescriptorNamespace::Dvb`, supplied by the caller through
  `new_with_namespace`. The default is `Unspecified`.
- **FFmpeg-compatible view.** Each observer is bound to one program, PID,
  stream type and stream ID (`NativePesScope`). Contexts outside the observed
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
  pinned FFmpeg reference; nonmatching cases are kept as failures, not
  normalised.

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
