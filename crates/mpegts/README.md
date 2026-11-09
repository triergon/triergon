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
carriers), `catalogue` (programs and streams), `dvb` and `input`.
Its `packet_completion` module re-exports the consumer policy in `compat::pes`.

`compat::pes::PacketCompletion` confirms that one physical input contained only
control traffic. Feed it every source event with its actual `SourceContext`,
seal the `TransportStream::media_entity_end` witness before logical EOF, then
observe source `Finished` and call `confirm_no_media`. A `NoMediaConfirmation`
requires contiguous physical carrier coverage and a matching core EOF. Only
PAT, associated PMT, null and no-payload carriers qualify; unexplained payload,
rejected transport, source PES and unfinished PMT sections remain refusals.
Malformed PSI and its diagnostics stay source facts rather than becoming
validated tables or a PES scope. `source_diagnostics()` retains those faults.
Use `required_work()` and `required_end_work()` for admission grants; an
insufficient grant leaves the event unconsumed. Errors after admission are
sticky, and cancellation cannot publish a confirmation.

`compat::pes::TransportFaultRecovery` wraps a drained, unbound `DemuxObserver`.
Ordinary input keeps the same assembly. Its opt-in fault path admits AVC-signalled
TS188 with exactly one TEI or scrambling fault, one complete nonempty finite PES,
and valid PTS-only encoding. Feed the selected carrier through `push`, then its
actual source diagnostic through `confirm_source_fault`. Until that confirmation,
`poll` retains the carrier and returns `NeedPayload`. Combined faults, incomplete
PES, other headers and entity or epoch transitions remain unsupported.

`TransportObservation::Faulted` retains the unchanged carrier, source diagnostic,
event contexts, byte ranges and charged storage. Its payload may be damaged or
scrambled: it is neither validated source PES nor a codec access unit.
`DemuxClock::project_transport_fault` places the encoded PTS in the existing demux
interval without creating a source clock anchor; encoded DTS remains absent.
The policy allocates two charged one-entry descriptor tables and aliases the
original payload. Cancellation and allocation pressure preserve pending input.
An unavoidable storage total above the pool limit returns `Limit`; storage held
by external outputs or reservations can instead cause retryable `Backpressure`.

`TransportFaultRecovery::with_policy` adds explicit `PesFaultPolicy` choices for
isolated PES timing-header faults and fully checked PCR/OPCR rejections. The
default constructor keeps all additional choices disabled. Accepted carriers with bad
timing encoding must receive their matching later PES diagnostic before output.
Rejected carriers must have only the selected clock fault, complete syntax
inspection and no secondary error; their original bytes and rejection remain
in `TransportObservation::PesFault`. Each admitted PES must be complete, finite
and nonempty in one AVC-signalled TS188 carrier. No source clock, accepted
transport packet or source PES is reconstructed.

`PesFaultHeader` keeps raw header bytes, timing flags and optional encoded PTS.
Missing marker bits can have a separate compatibility value; forbidden timing
flags leave timestamps absent even if opaque optional bytes resemble a PTS.
`DemuxClock::project_pes_fault` retains its existing interval when time is absent.
It never updates the source clock. Fully checked rejected clock carriers retain
their counters without claiming source continuity. Additional damage, partial
PES and unknown clock/header contexts remain refusals.

`PesFaultPolicy::source_pcr_pid` binds a validated source PMT's PCR PID. For
selected AVC, a drained TS188 PUSI carrier carrying valid PCR and a discontinuity
indicator may emit a complete finite packet in its original source epoch. The
actual later source `Timeline` must confirm exactly the next clock epoch through
`confirm_source_timeline` before more input or EOF. Configuration and entity stay
the same. Confirmation consumes its source event ordinal, so later carriers
cannot reuse it. Only future packet source epochs change; the existing `DemuxClock`
interval stays intact. A missing Timeline or quarantined clock remains a refusal.

`PesFaultPolicy::terminal_transport` can preserve completed nonempty finite AVC
packets before one final partial or sync-invalid TS188 carrier. Its original
transport diagnostic must immediately follow the last admitted carrier, with no
unfinished PES. Bind the original core cursor with `bind_source_input` before
admitting any carrier. Late binding and replacement bindings are refused. The
same pool, source, generation and sequential request must supply both terminal
confirmations; a fresh cursor cannot replace a failed one. `hold_terminal_transport`
only stages that diagnostic. Real
`TransportStream::media_entity_end` and core EOF must seal the exact final extent
through `seal_terminal_end`, followed by source `Finished` and another core EOF
check through `confirm_terminal_finished`. Only then does `terminal_confirmation`
expose a `TransportTailConfirmation` and permit logical completion. The diagnostic
and any rejected bytes stay available as charged storage. No empty output,
additional carrier, malformed adaptation/PES, entity transition or I/O failure is
promoted to success. These boundary checks add fixed state and no allocation,
payload copy or descriptor table; a rejected carrier shares its existing charge.

`compat::pes::MalformedInputCompletion` can report selected TS188 AVC carriers
with reserved adaptation control, an adaptation overrun, an invalid PES start
code or a PES header exceeding its declared length. It requires a real active
source PMT and drained, input-bound observer. Accepted transport with malformed
PES waits for its matching source diagnostic; rejected transport retains the
original rejection. Each `MalformedInputFact` keeps the original charged bytes,
source fault and event contexts. The source parser still reports the refusal.
A later valid PES start is forwarded to the observer. Continuations after a
discard require a new start before their packet disposition can be proved.

The policy binds the original core cursor before the first byte and advances
its `TransportStream` through `poll_source`. Each issued event borrows that
operation and cursor. The zero-packet branch accounts for every event and
admits only verified self-contained control sections or typed discards.
`NoPacketsConfirmation` requires physical End and Finished from the same
operation and a fresh EOF check on the original cursor.
The completion can account for an empty physical input at EOF and preserves
the original PAT, PMT and PES scope. It requires contiguous input coverage and
a healthy cursor. `required_work` bounds per-event admission. A short grant
leaves the event available for retry. Fixed policy state and charged aliases
reuse the parser's original bytes.

## Access units

`Reader::new(&pool, source, capabilities, ReaderLimits::default(), input_window)`
opens a sequential TS188 source. Set `ReaderLimits::format` for M2TS192, and
`program` to select a program explicitly. Without a selection, the PAT must
contain exactly one media program. `ContainerReader` supplies the normal
input-window, work, cancellation, backpressure and reset protocol. Seeking
returns `Error::Unsupported` without changing reader state.

Repeated sections that match the active PAT or PMT retain each occurrence's
source ranges. The reader reuses the accepted table when the bytes match.

`Reader::step_with_carriers` delivers transport and ordinary continuity
observations to a borrowed callback in source order. A `TransportObservation`
exposes the original bytes and location. Call `to_packet` to retain the complete
carrier and its parsed fields. Complete TS188 continuations can move their
payload backing directly into a prepared PES span. Complete carriers can also
be inspected from a charged copy when the input window cannot be shared.
The method returns the same
access units, other source events and progress steps under the caller's work
grant. `compat::annex_b::ContinuityReader` keeps the carriers its proof needs.
The elementary video reader can append interior NAL bytes directly from a
borrowed PES span. It uses the streaming path at possible NAL boundaries.

For an ordered stream, call `Reader::finish_entity` with the final input
token and the next entity's distinct source ID and capabilities. The reader
consumes the last supplied bytes and requests offset zero of the next entity
with the same generation and a new request number. `EntityTransition::Contiguous`
carries the current tables, continuity counters, partial PES and access-unit
state forward, including from an initialization section. An explicit
`Discontinuity` reports unfinished access units and clears the track catalogue
until the next entity's PAT and PMT arrive. Source ranges keep each entity's
physical coordinates. Call `finish` on the last entity to drain the logical
stream. A reset starts the current entity again with fresh transport state.
The boundary semantics follow [RFC 8216 sections 3 and 4.3.2.3](https://www.rfc-editor.org/rfc/rfc8216#section-3).

The admitted carriages are AVC and HEVC Annex B, single-block ADTS AAC-LC,
LATM in LOAS, AC-3 and E-AC-3 with explicitly declared DVB or ATSC signalling,
and DVB subtitle segments.
For ATSC, set `ReaderLimits::descriptor_namespace` to `DescriptorNamespace::Atsc`
to interpret `0x81` as AC-3 and `0x87` as E-AC-3. Both require PES stream ID
`0xbd`. ATSC E-AC-3 supports a six-block independent syncframe with its
dependents within one PES. Shorter independent syncframes, which require
assembling multiple groups into one ATSC access unit, are unsupported.
Tracks report the codec signalled by the selected namespace and preserve the
original stream type as the codec tag. The PMT retains the descriptor bytes.
See [A/53 Part 3 §5.5.2 and §5.7](https://www.atsc.org/wp-content/uploads/2023/02/A53-Part-3-2023-02.pdf)
and [A/52 Annex G §3.3](https://www.atsc.org/wp-content/uploads/2021/04/A52-2018.pdf).

Existing framers delimit the complete source bytes before decoder-feed conversion.
MPEG audio/video, other subtitle carriages and ambiguous private streams currently emit
`ReaderEvent::Unsupported`. Their original PMTs and PES remain available as source events.

The DVB E-AC-3 lane requires PMT stream type `0x06`, an elementary
`0x7a` descriptor and PES stream ID `0xbd`. A matching elementary
`EAC3` registration can corroborate the descriptor. Automatic probing
leaves the descriptor namespace unspecified. The lane admits one
independent substream (ID zero) followed by up to eight ordered dependent
syncframes at the same sample rate and block count. Converted independent
syncframes carry no dependents. Additional independent services and mixed
AC-3/E-AC-3 elementary streams are outside this lane. Each group ends at
the next validated independent syncframe or clean logical EOF. An interrupted
or truncated pending group follows the existing interruption or audio EOF policy.
The reader can assemble a complete syncframe from the spans of one PES directly.
Syncframes split across PES use the streaming frame reader.
`Ac3AccessUnit::syncframes()` returns all syncframe facts in order and
`Ac3AccessUnit::bytes()` returns the complete group. Its sample span and PES
anchor use the primary syncframe. `maximum_audio_frame_bytes` bounds each
syncframe. The group ceiling is nine times the lower of that limit and the
4096-byte wire maximum, giving 36,864 bytes with the default settings. The shared
budget charges retained
syncframes, group coalescing and metadata. The retained-PES limit includes
boundary lookahead. EOF fragment reporting may retain one additional
unfinished syncframe.
Signalling follows ETSI TS 101 154 and EN 300 468 Annex D.5. Syncframe
grouping follows A/52 Annex E §2.3.1.

`ReaderLimits::unframed_audio_policy` can select
`compat::pes::UnframedAudioPolicy::ReportAtEof` for the private-audio carriage
view when the source namespace is unspecified. It requires unambiguous PMT
evidence and exactly one nonempty, finite `0xbd` PES in an uninterrupted byte
run. The payload must fit `maximum_audio_frame_bytes` and contain no AC-3 or
E-AC-3 syncword or trailing syncword prefix. At logical EOF,
`ReaderEvent::UnframedAudio` carries the original payload, PES ranges and source
clock separately from complete packets. The track retains its source tag,
codec and namespace. Declared DVB and ATSC carriages use their syncframe readers.

For ADTS, `compat::adts::PacketPolicy::ReportFragments` opts into bounded
parser framing. An aligned, complete AAC-LC envelope still produces a packet.
Opaque bytes before a following envelope, unselected envelopes and logical EOF
tails produce `ReaderEvent::AdtsFragment`. Each fragment keeps the observed
boundary, original PES ranges and an applicable source clock, with no sample
duration. The policy is selected with `ReaderLimits::adts_packet_policy`.
The default aligned parser follows `AudioEofPolicy` for unfinished audio.

`ReaderLimits::adts_pes_policy` can select
`compat::adts::PesPolicy::ReportUnbounded` for stream type `0x0f` and audio
PES stream IDs `0xc0`–`0xdf`. A zero packet length violates the transport PES
rule in H.222.0 §2.4.3.7. The strict reader reports that diagnostic. With the
policy selected, the remaining header is validated and original bytes are
assembled up to the next PES start or logical EOF, within the transport limits.
Span-table growth charges its allocation and copy across work grants. A
blocked replacement audits the retained source, transport tables and
access-unit lanes across work grants, counting each shared backing once. A
replacement that cannot fit in the pool reports `Limit`. Storage held only
by the caller can cause retryable `Backpressure`.
`TraceEvent::UnboundedAdtsPes` retains the wire violation and source spans.
The selected ADTS framer still determines the output boundaries.
`ReaderEvent::RecoveredAdts` retains units that use the rejected source for
payload or timing, separately from the neutral packet stream.

LATM stream type `0x11` with audio PES stream IDs `0xc0`–`0xdf` defaults to
aligned LOAS AudioSyncStream framing (ISO/IEC 14496-3 §1.7.2). A
`LoasFrame` retains the complete envelope and its PES ranges across carrier
and PES boundaries. The LATM multiplex element is opaque. The reader applies
`maximum_audio_frame_bytes` and preserves available PES timestamps without
inferring duration or decoder configuration.

`ReaderLimits::latm_packet_policy` can select
`compat::latm::PacketPolicy::PreservePes` to report each nonempty validated
PES as `ReaderEvent::LatmPes`. This follows the pinned demuxer's packet
boundaries even for a PES with several or incomplete LOAS envelopes.
The observation retains the original PES spans and stays outside the neutral
packet stream.

DVB subtitle carriage uses the PMT's `0x06` stream type and subtitling
descriptor (`0x59`) in the declared DVB namespace. One `AccessUnit::DvbSubtitle`
contains the ordered segment headers and bodies from one finite, aligned
`0xbd` PES carrying PTS. The `0x20 0x00` wrapper and terminal `0xff` are
validated and retained in the original PES. Segment types and page IDs are
preserved as bytes. Each PES is framed independently, including PES packets
with the same PTS. Display-set assembly and segment decoding are separate.
PES spans remain charged, and scattered segment bytes are copied cooperatively
into charged storage. Framing follows ETSI EN 300 743 §§6.2 and 7.2.

An overlong final segment fails with `Truncated` by default. With
`DvbSubtitleTailPolicy::ReportCompletePrefix`, a validated PES with a complete
final segment header and an unfinished body can instead produce a
`ReaderEvent::DvbSubtitlePrefix` containing only its preceding complete segments.
At least one preceding segment is required. The event keeps the entire PES and
its original clocks, and `DvbSubtitleUnit::truncated_tail()` gives the range of
the unfinished segment in elementary coordinates. Invalid wrappers, incomplete
segment headers and missing end markers at segment boundaries fail.

`Packet` implements `PacketInfo` and retains its exact PMT, source configuration epoch,
framer syntax and intersecting PES events. `sources()` maps its elementary-byte
extent to retained PES events, including all physical source and header ranges.
Framer coordinates use a private concatenated elementary view (`SourceId(0)`),
not physical file offsets. Video scatter payloads are coalesced cooperatively
into charged immutable storage. Cloning a packet or `SourceEvent` shares its
charged storage; it never performs an infallible vector clone.

`Track::configuration_epoch()` comes from its elementary route, which can be
reused across PMT versions and PCR binding changes. In-band SPS/PPS changes remain
in the source NALs and do not advance it. Consumers track parsed codec-configuration
changes separately.

`ReaderEvent::Transport` delivers each complete original carrier, and
`ReaderEvent::Continuity` reports `First` and `Continuous` counter observations.
`ReaderEvent::Source` delivers the remaining trace events, including continuity
transitions. All are delivered in source order. PMT track catalogues change
atomically with the corresponding source event. A PMT, clock or interrupted-byte-run
boundary reports unpublished bytes through
`ReaderEvent::Interrupted`, rather than treating a loss boundary as EOF.
Logical EOF drains the framers before the original `Finished` event and final
`Step::End`. When a split PES header needs a larger source-range table, the
reader checks the allocations it must retain across that growth. If they cannot
fit together, it reports a PES `Limit` diagnostic. Memory held by caller outputs
can instead cause retryable `Backpressure`, with the original header and byte
ranges preserved on retry.

Audio defaults to `AudioEofPolicy::RejectIncomplete`: an unfinished
frame still returns `Truncated`. With `AudioEofPolicy::ReportIncomplete`, only
logical source EOF can instead produce `ReaderEvent::IncompleteAudio` carrying
`AccessUnit::IncompleteAudio`. It keeps the exact unfinished audio bytes,
truncation reason, validated PES ranges and any applicable one-use source anchor.
It has no complete-frame duration and is never a neutral packet or complete AU.
Fragment copying follows the applicable audio extent limit, cooperative work
and the shared storage budget. Retained fragments survive reader reset or closure. Clock,
program and loss boundaries remain interruptions; transport failures and invalid
headers are not recovered by this reporting policy.
All framers finish before EOF fragments are reported. Their publication counts
the remaining reader tables and distinct backing allocations against the storage
limit; caller-held output and external reservations can still cause retryable
backpressure.

Video defaults to strict Annex B framing. With
`compat::annex_b::VideoEofPolicy::ReportUnframed`, a final AVC or HEVC byte run with invalid
leading bytes and no Annex B start code produces `ReaderEvent::IncompleteVideo`
at logical source EOF. It retains the validated PES spans and first source
clock anchor. Its byte extent follows the video access-unit limit. The event
is separate from the neutral packet stream. An interruption ends the run,
and a later start code within a run preserves the leading-byte refusal.

`compat::annex_b::ContinuityReader` implements `ContainerReader` over one
original source reader. It can observe an unframed run through selected AVC
or HEVC continuity changes in a single entity. Its separate
`ContinuityEvent::Residue` carries the original logical end, contributing
carrier spans, header clocks, and the original continuity and diagnostic
evidence. `ContinuityEvent::Pending` still delivers the original event while
its proof is in progress. The adapter honors the caller's work grant and
retains the existing source stream when the optional proof is declined.
Track catalogue changes accompany the source event that publishes them.
Truncated completion drops pending proof state. A stale completion can be
retried with the active input request.

`compat::opaque_video::OpaqueRecovery` replays one TS188 entity for a supported
transport, PES timing or finite-length diagnostic on a selected AVC stream.
It reproduces the diagnostic from the replay cursor and requires physical
End and source Finished. The reported run retains the exact charged payload
spans and fault facts. A start code anywhere in the run preserves the refusal.

Annex-B groups without an admitted primary/base-layer picture
remain `ReaderEvent::VideoHeaders`,
outside the AU packet stream. Packets and events remain usable after close or reset.

A completed PES whose header predates a program boundary remains available as
`ReaderEvent::InterruptedPes`, together with the boundary event. Framing resumes
with PES that start after the boundary. This keeps a retained PES from inheriting
a newer PMT or PCR binding.

Complete transport carriers can share one bounded input allocation per
entity. Split carriers and oversized backing allocations use the carry
buffer. The same bound applies when replaying a source fault.

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
- The [access-unit comparison](../../harness/features/mpegts-access-units/contract.json)
  includes an intact coded HEVC and 32 kHz E-AC-3 transport and six DVB subtitle
  inputs, one with a truncated final segment. Their requests explicitly declare
  the DVB namespace. The comparison opts in to reporting the subtitle prefix.
- The source view is compared field by field with an independent oracle over
  the corpus. The FFmpeg-compatible view is compared packet by packet
  (payload bytes, timestamps, flags, positions, stream-ID side data) with the
  FFmpeg reference; nonmatching cases are kept as failures, not normalised.
  `cargo xtask report` runs that comparison from
  [`harness/features/mpegts-pes/`](../../harness/features/mpegts-pes/contract.json)
  through `playback-worker`'s `ts-` modes, which use the public API only:
  `TransportStream` selects the program, its first active PMT binds a
  `compat::pes::DemuxObserver` to the one elementary stream, wrapped by
  `TransportFaultRecovery`, and `DemuxClock` places the timestamps. A typed
  program-policy or empty-observation refusal
  with no emitted packets and one entity can complete through a bounded
  control-only replay and a real `NoMediaConfirmation`. Other refusals remain
  failures unless the separately reported malformed-input policy proves complete
  original-cursor coverage and real EOF with `NoPacketsConfirmation`. That
  proof runs on the original source operation. The control-only replay runs
  inside the measured operation.
  For an ordered sequence, each intermediate physical End is sealed before
  the same cursor advances to its next request. Ordinary boundaries retain
  PES assembly and the demux clock, including an initialization entity's
  program map. A declared discontinuity requires drained PES assembly and
  a new source map for the same elementary stream. Folded positions use the
  sequence coordinate system. Source ranges and epochs remain physical
  source facts.
  The 272 recorded requests are 97 compat-view operations. The AArch64 Linux
  gate build at `5634f47d`, with Rust 1.99.1 and default flags, checks all 97
  against the pinned FFmpeg formats worker and the unchanged recorded expected
  folds: 81 match, including 15 control-only inputs. Full source and evidence
  hashes are in [status](../../docs/status.md#measured-revisions).
  The 168 requests for the strict source view, 64
  of them for its native clock, are replayed as the compat-view operation on
  their inputs: the strict source view and its native clock are not compared.
  That archived comparison's 16 unmatched operations cover clock and format
  boundaries, recovery of rejected and damaged carriers, input across media
  entities, and malformed
  transport or PES whose empty result the completion policy cannot prove. They
  are `pcr-timeline-discontinuity`, `pcr-extension-invalid`, `transport-tei`,
  `transport-scrambled`, `transport-afc-reserved`,
  `transport-adaptation-overrun`, `transport-partial-final-packet`,
  `transport-sync-loss`, `pes-timestamp-marker`, `pes-forbidden-time-flags`,
  `pes-optional-header-overrun`, `pes-start-prefix-invalid`,
  `hls-contiguous-pes-carry`, `hls-external-map`, `hls-explicit-discontinuity`,
  `opcr-without-pcr-invalid`. These PES comparisons do not qualify source access
  units. All 81 matching operations were measured with 30 paired samples on a
  c8g.large pinned to CPU 1. Their worst runtime upper bound is 1.0315 of the
  reference; peak heap is at most 0.073 and total resident RSS at most 0.980 of
  it. Every measured case passes those numeric gates. The 16 parity failures
  keep the feature unaccepted.

A separate live comparison of the finite source-fault policies on macOS
(`aarch64-apple-darwin`, source `f3f439b803e079208c9ab422270a77feb6281207`)
checks all 97 operations and 272 requests against the pinned FFmpeg formats worker
and unchanged recorded folds: 87 match and 10 remain failures. That source has no
native timing or memory qualification; the native figures above apply to `5634f47d`.

The source-boundary policies at `e7e1102c0afd466af24e9e8b969e823d5772a230`
have a separate live macOS (`aarch64-apple-darwin`) comparison against the pinned
FFmpeg formats worker and unchanged recorded folds: 90 of 97 operations match,
with all 272 requests retained. The four malformed empty-reference operations
remain refusals, and three operations across media entities lack a single-input
reference equivalent. No timing or memory was collected for this source. These
PES folds do not qualify the strict source clock or access units.

The ordered-input PES comparison at `576cf0a0` matches all 97 operations
on macOS AArch64 and preserves all 272 requests. This includes the three ordered
inputs and four malformed completion cases. Runtime and memory on native
AArch64 Linux remain open for this revision.

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
