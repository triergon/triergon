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

- **Source view** (`Reader` and the modules `transport`, `psi`, `pes`, `stream`
  and `clock`). Every value is a wire fact: raw 33-bit PTS/DTS, raw PCR,
  descriptor bytes, source byte ranges. Unknown DTS stays unknown.
- **FFmpeg-compatible view** (`compat::pes`, `compat::input`). It reproduces
  the packets, positions, flags and timestamps of FFmpeg's MPEG-TS demuxer,
  including its handling of damaged and headerless PES. It is fed from the same
  transport packets and never rewrites source facts.

## Modules

The crate root holds the access-unit reader (`Reader`, `ReaderEvent`,
`ReaderLimits`, `Track`, `Packet`, `AccessUnit`) and the shared source facts
(`PacketFormat`, `ParseLimits`, `SourceRange`, `SourceEpochs`, `AssemblyStep`).
Everything else lives in its module:

- `transport`: packets (`TransportPacket`, `Adaptation`, `Pcr`, `parse_transport`),
  framing (`TransportFramer`, `FrameStep`, `FrameState`, `NullCarrierRun`),
  continuity (`Continuity`, `ContinuityOutcome`) and in-place carrier inspection
  (`TransportInspector`, `TransportInspection`, `TransportObservation`,
  `RejectedTransport`).
- `pes`: PES assembly (`PesAssembler`) and its output (`Pes`, `PesHeader`, `PesSpan`).
- `psi`: sections (`SectionAssembler`), PAT and PMT tables, and descriptors.
- `stream`: the selected-program source operation (`TransportStream`), its events
  (`TraceEvent`, `TraceFault`, `FaultKind`, `CompactTraceEvent`,
  `CarrierObservation`), feed delivery (`FeedItem`, `FeedCarrier`,
  `FeedPayloadRun`, `FeedContinuation`), source witnesses (`ObservedTrace`,
  `MediaEntityEnd`) and a borrowed view of the program map it routes by
  (`ActiveProgram`, `ProgramStream`, from `TransportStream::active_program`).
- `reader`: the root reader types with their policies (`AudioEofPolicy`,
  `DvbSubtitleTailPolicy`), entity plans (`EntityTransition`, `SeekEntity`),
  source events and clocks (`SourceEvent`, `PacketClock`, `PayloadSource`) and
  access units (`Ac3AccessUnit`, `DtsAudioFrame`, `DtsExtensionFrame`,
  `DvbSubtitleUnit`, `LoasFrame`, `MpegAudioFrame`, `IncompleteVideo`,
  `UnframedAudio`).
- `clock`: raw timestamps lifted to exact time (`source_timestamp`, `SourceTimePolicy`).
- `mux`: the single-program writer ([Writing TS](#writing-ts)).
- `compat`: FFmpeg-compatible policies, one module per family: `pes`, `stream`,
  `input`, `adts`, `latm`, `dts`, `annex_b` and `opaque_video`.
- `reference_trace` (non-default feature `reference-trace`): the projections that
  exist only to compare traces with FFmpeg, in `recovery` (packets from rejected
  carriers), `catalogue` (programs and streams), `dvb` and `input`.

`compat::pes::PacketCompletion` confirms that one physical input contained only
control traffic. Poll the source with `TransportStream::poll_observed` and
pass each issued event to `observe_issued` with its `SourceContext`. Seal the
`media_entity_end` witness before logical EOF, then observe `Finished` and call
`confirm_no_media`. A `NoMediaConfirmation` requires contiguous carrier coverage
and matching events and EOF from the same parser and input cursor. Only
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

`RecoveredPes::Faulted` retains the unchanged carrier, source diagnostic,
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
in `RecoveredPes::PesFault`. Each admitted PES must be complete, finite
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
diagnostic and both terminal confirmations must come from the same parser,
pool, source and cursor request. `hold_terminal_transport` stages a diagnostic
issued by that parser. Real `TransportStream::media_entity_end` and core EOF
must seal the exact final extent
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
input-window, work, cancellation, backpressure and reset protocol.

`Reader::request_seek` scans one seekable, length-known entity for the previous
coded AVC or HEVC random-access picture, or a timestamped AAC-LC in ADTS or
LOAS, AC-3,
E-AC-3, MPEG audio or DTS core access unit. Layer III requires an empty bit
reservoir at the landing. DTS core requires a frame that needs no previous decoder
history. LOAS requires a complete in-band LATM configuration with one AAC-LC
payload and indexed sampling frequency. Subsequent dependent frames are
delivered as preroll. The landing is the
last eligible packet with the greatest presentation time at or before the target.
Audio frames without a source PTS are delivered
with their original timing when they follow a landing.
`ReaderLimits::maximum_seek_scan_bytes` limits the scan to 64 MiB by default.
Packets retain their original bytes, clocks and source ranges. The reader retains
a bounded single-track suffix when possible and otherwise replays the entity.
An `Event::Seek` precedes the landing packet and carries the requested
discard time for `PlaybackPreroll`. The caller supplies decoder setup.
The reader refuses seeks across selected-track source epochs and ambiguous clock
wraps. A coded random-access picture must have resolved timing.

`Reader::request_seek_in_epoch(request, epochs)` selects the exact clock and
configuration pair reported by `Packet::epochs()` in the same source. Only
access units in that pair compete for the landing; equal or overlapping timestamps
in other epochs do not compete. A missing epoch or an epoch with no eligible
preceding access unit is refused. Clock-wrap ambiguity within the selected epoch
remains a refusal. These source facts are independent of input generations and
PMT version numbers.

This operation scans and replays from the start.
`request_seek_in_entities_in_epoch` accepts an ordered `SeekEntity` plan,
including boundaries inside a PES. Replay publishes the original PAT, PMTs and clock transitions, and
selected-epoch `VideoHeaders` before the landing, so decoder setup retains its
source order. The landing packet and the complete following suffix keep their
bytes, PTS, DTS, track configuration and PES ranges, including subsequent epoch
changes. The caller supplies any additional decoder configuration needed at the
landing. `request_seek_in_epoch_and_index` and
`request_seek_in_entities_in_epoch_and_index` cache a bounded index for the
selected pair. Reuse requires the same track, epoch pair and exact ordered source
identities and lengths. Equal landing times select the last eligible unit.
Unordered times or exhausted index bounds fall back to scanning. Explicit epoch
indexes replay from the start to reconstruct state. Timestamp-only requests never
reuse an explicit-epoch index. Sources must remain stable between requests.
Reader tests cover indexed epoch selection across TS188/M2TS192 entity plans,
codec changes and exact packet suffix facts; these combinations have no matched
reference or native runtime and memory qualification.

The epoch distinction follows [H.222.0 (08/2023)](https://www.itu.int/rec/T-REC-H.222.0-202308-S/en)
§2.4.3.5 (a signalled PCR discontinuity starts a new system time base) and
§2.4.4.9–10 (current program definitions and their changes). Selecting an epoch
is a reader policy; it does not alter those source facts or normalize timestamps.

The required [picture comparison](../../harness/features/mpegts-previous-picture/contract.json)
covers 36 explicit-epoch AVC seeks against the pinned FFmpeg formats build:
a clock change or same-codec PMT descriptor change, each in TS188 and M2TS192,
with both seek policies, ordered entity splits and repeated indexed seeks.
Equal-PTS controls put overlapping epochs in separate entities. Each picture has its own PES. Both workers scan the full
stable input through EOF and replay every suffix packet, comparing the requested,
landing and discard times, PTS, effective DTS, random-access flags and all bytes.
The candidate selects the source epoch pair; the reference selects landing
candidates by its public packet positions over the authored physical boundary.
After building the workers, run:

```sh
CARGO_INCREMENTAL=0 cargo xtask report \
  --contract harness/features/mpegts-previous-picture/contract.json \
  --manifest harness/features/mpegts-previous-picture/corpus.json --corpus .
```

Native Linux parity, runtime and memory qualification remain separate checks.

DTS-HD seeking also supports a lossless XLL stream with no core, one extension
substream at index zero, one presentation and one active speaker asset. An entry
must be advertised as construction 17, carry its own static metadata, use coding
mode 1, and contain one complete
version 1 XLL frame. Its navigation must signal a DWORD-aligned XLL sync word at offset zero,
zero initial decoding delay and no smoothing buffer. The common-header checksum
is checked cooperatively before the seek result is issued. Missing static setup
or sync, nonzero delay and nonzero sync offset do not establish an entry. Other
asset layouts, mixing, speaker remapping, secondary decoding and Rev2 DRC are
unsupported for seeking. Construction 14 additionally supports a separate canonical
core without internal extensions, with a clear predictor-history flag and DWORD-aligned
extension start. The extension must satisfy the same XLL recovery checks and explicitly
map its backward-compatible core to extension zero and asset zero. Core history alone
does not establish recovery. The same scan bound, index, original packet
suffix and preroll discard time apply.

The recovery requirements follow [ETSI TS 102 114 V1.6.1](https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.06.01_60/ts_102114v010601p.pdf),
Tables 7-2 and 7-5–7-7 and §7.5.3.4. XLL predictor history is confined to a frame
(§8.5.2.1 and §8.5.4); the common header and checksum are specified in §8.3.1 and
Annex B. Annex G.3.1 and Table E-2 identify construction 17 as XLL without a core and
construction 14 as a separate core plus XLL.
Annex F.4.3 defines PES alignment and complete access-unit carriage;
alignment alone does not establish decoder recovery.

The next recovery proofs require all active assets of every selected presentation
and extension substream to be independently available, with their navigation and
core dependencies checked together. Inherited static metadata needs a verified
setup origin included in preroll; PES alignment cannot supply it. Delays and
smoothing need a bounded earlier decoding origin, while nonzero XLL sync offsets
need a complete frame and checked alignment at the signalled offset. Speaker
remapping and mixing need their full mapping and scaling dependencies checked.
Legacy DRC and dialogue metadata are parsed within the asset descriptor; secondary
decoding and Rev2 DRC remain unsupported. Authored construction-14 controls test
packet suffixes and recovery metadata. The pinned transport reference has no DTS
decoder, so these controls do not establish decoded-PCM parity. Native runtime,
heap and kernel-bound RSS qualification for this seek profile is unmeasured.

Use `Reader::request_seek_in_entities` for consecutive length-known, seekable
parts of one logical transport stream. The ordered `SeekEntity` list supplies
source IDs and lengths, and the scan bound applies to their combined size. Feed
input by the requested source ID and finish the final entity. It is also safe to
finish an intermediate entity as soon as all its bytes are supplied. The reader
advances those boundaries during scan, replay and delivery, preserving in-flight
PES and access units. The ordinary single-entity method requires a single source.

Call `Reader::request_seek_and_index` to collect a bounded timestamp index
during the first scan. Later seeks to the same track can use the index to select
a landing with no separate scan. The index preserves the last
packet on tied timestamps. It falls back to scanning when anchors are out of
order or the entry or storage limit is reached. The source must remain stable.
Reset, close and continuation outside a seek plan discard the index.
`Reader::request_seek_in_entities_and_index` builds the same bounded index over
a contiguous entity plan. A later seek with exactly the same IDs, lengths and
order can reuse it for the same selected track.

For one clear TS188 or M2TS192 entity containing a single supported elementary stream,
the index can also retain physical restart checkpoints. The source must have
one current PAT section, one unchanged PMT and ordinary continuity without
duplicates, discontinuities or transport, configuration or clock faults.
Only PAT, its PMT, the elementary PID and null carriers are admitted. The PCR
PID must be the elementary PID or absent. A checkpoint requires consecutive,
complete PAT and PMT carriers immediately before a timestamped PES.

The independently framed profiles are:

- MPEG-1/2 Layer I or II audio: exactly one complete frame with an indexed
  bitrate, including its header and payload in one finite PES of at most 32
  consecutive carriers.
- AVC: one progressive IDR picture with its referenced SPS and PPS in the same
  aligned, finite PES. Up to eight ordered I slices are admitted, starting at
  macroblock zero and identifying the same picture.
- HEVC: one base-layer, single-temporal-sublayer IDR_N_LP slice with its
  referenced VPS, SPS and PPS in the same aligned, finite PES.

Video may have an initial access-unit delimiter, but no SEI or other NAL types.
Each video PES spans at most 32 consecutive carriers and contains exactly the
access unit's original bytes. Parameter-set and slice headers establish the
local setup chain and independence; framing does not validate decoded pixels.

The first scan validates the whole entity. Each checkpoint binds the original
source ID and length, all complete PAT/PMT/PES carrier records (including opaque M2TS prefix bytes), independently
parsed PES header and payload ranges, the original active tables, the elementary
byte coordinate and the preceding access-unit and NAL counts. At most
`maximum_entries` checkpoints are allocated under the shared budget. Later
requests start at the recorded PAT's physical offset and read no earlier bytes.
The reader compares every checkpoint carrier byte for byte before delivering
the landing, reconstructs transport state from those in-band tables, and
restores the original elementary/PES coordinates, source clocks, table
provenance and output limits. The caller resets its decoder at the independent
access unit; the resumed bytes carry its configuration. Changed checkpoint
bytes are a source mismatch. Repository-generated M2TS192 Layer I and II
controls compare forward and backward repeated seeks with reference scan/replay
through EOF; reader tests prohibit prefix reads and compare original PES ranges
and complete packet facts for those same inputs. Video framing can read into
the following PES to establish the final NAL boundary.
This bootstrap follows H.222.0 §§2.4.3.3, 2.4.3.6–7 and 2.4.4, H.264
§§7.3.2.1–2, 7.3.3 and 7.4.1.2.4, and H.265 §§7.3.2.1–3, 7.3.6.1 and 7.4.2.4.4.

Other landings replay the entity prefix to reconstruct transport and decoder
framing state. This includes offsets without a local table/PES proof,
audio PES spanning more than 32 carriers, free-format or Layer III audio, video needing prior
setup or carrying SEI or other IRAP types, and contiguous entity plans.
Checkpoint storage refusal preserves replay; pressure during a direct restart
drops the optional index and restarts replay. Sources must remain stable between
requests: the bounded byte comparison does not revalidate an unread prefix or
hash the whole entity. `InputRequest::offset` identifies the actual physical
read; `SeekMethod::Index` and `replays_from_start` alone do not establish direct
source access.

Use `Reader::new_with_packet_tracks` when the caller has already chosen the
elementary PIDs. Only those PIDs allocate access-unit framers and produce
packet events. Track metadata, original PES, and transport diagnostics remain
visible for every stream. Reset keeps the selection until the reader is closed.
A closed reader with a selection needs to be constructed again.

Repeated sections that match the active PAT or PMT retain each occurrence's
source ranges. The reader reuses the accepted table when the bytes match.

`Reader::step_with_carriers` delivers transport and ordinary continuity
observations to a borrowed callback in source order. A `transport::TransportObservation`
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

The admitted carriages are AVC and HEVC Annex B, MPEG-1 and MPEG-2 audio,
single-block ADTS AAC-LC, LATM in LOAS, AC-3 and E-AC-3 with explicitly
declared DVB or ATSC signalling, DVB-signalled DTS core, and DVB subtitle segments.

For MPEG audio, PMT stream types `0x03` and `0x04` select Layer I, II and III
frame parsing in audio PES packets (`0xc0`–`0xdf`). The reader can assemble a
frame across PES boundaries and retains its original bytes and PES ranges.
`MpegAudioFrame::header` exposes the coded layer, rate and sample count.
The packet's sample duration is exact. When the bitrate index is zero, the
reader confirms two consecutive frame spacings with matching version, layer
and sample rate. It accounts for the original padding slots and bounds each
frame by `ReaderLimits::maximum_audio_frame_bytes`. The header exposed to the
caller retains the zero bitrate index. MPEG-2.5 rates are unsupported.
See [H.222.0 Tables 2-22 and 2-34](https://www.itu.int/rec/T-REC-H.222.0-201808-S/en).

For DTS core, select a DVB descriptor namespace and carry the DTS descriptor
(`0x7b`) on a private-data PMT entry (`0x06`). The reader frames canonical normal
DTS core packets, including headers and bodies split across PES packets.
`DtsAudioFrame` carries the exact sample span and original frame bytes. The
packet retains its source clocks and PES ranges.
See [ETSI EN 300 468 Annex G](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf).
If the PMT has empty program and elementary descriptor loops, a caller that
knows the private-data stream contains DTS core can supply its program and PID
in `ReaderLimits::unlabelled_dts_core`. The reader requires PES stream ID
`0xbd` and validates each DTS frame. Track metadata identifies DTS while
retaining the original PMT and stream type.

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
MPEG-1/2 video, other subtitle carriages and ambiguous private streams currently emit
`ReaderEvent::Unsupported`. Their original PMTs and PES remain available as source events.
For DVB DTS-HD signalling, `psi::Descriptor::dvb_dts_hd` reads Annex G.3
substream and asset information from the original descriptor bytes.
`ElementaryStream::dvb_dts_hd` looks up the descriptor on a DVB private-data
track and rejects conflicting DTS core and DTS-HD descriptors. The access-unit
reader can frame one core-bearing DTS-HD unit per aligned finite PES. It
checks the signalled extension substream order, extents and header CRC and
leaves the asset payloads intact. `DtsAudioFrame::core_bytes` exposes the core
prefix, and `bytes` exposes the complete unit. The original PES supplies its
clock.
A track without a core can carry one to four signalled extension substreams.
Each `DtsExtensionFrame` contains one complete access unit, including all its
extension headers, zero alignment padding and opaque asset bytes. The reader
checks every extension's index, extent and header CRC before publishing the unit.
Only a single-substream track can contain multiple access units per aligned
finite PES; a multi-substream track requires exactly one complete unit per PES
([TS 102 114 V1.6.1 §7.5.2 and F.4.3](https://www.etsi.org/deliver/etsi_ts/102100_102199/102114/01.06.01_60/ts_102114v010601p.pdf)).
The lowest-index extension supplies the coded duration when its reference clock
and frame duration are available. Later units can reuse that substream's static
timing. `Packet::sample_duration` preserves the exact rational span; the integral
90 kHz duration stays absent when that span is fractional. Only the first unit
claims the original PES clock, and missing DTS remains absent. Every unit retains
its intersection within the original PES. The aligned PES limit is
`ReaderLimits::maximum_dts_hd_pes_bytes`.

The [grouped extension comparison](../../harness/features/mpegts-dts-hd-grouped/contract.json)
explicitly feeds original PES bytes and source clocks to FFmpeg's DTS parser.
The [multiple-extension comparison](../../harness/features/mpegts-dts-hd-extensions/contract.json)
compares the demuxer's complete PES access units: the explicit parser emits each
extension separately. Both keep the existing [DTS-HD demux comparison](../../harness/features/mpegts-dts-hd/contract.json)
intact. Comparison projections do not alter public packets.

For DVB AC-3, an elementary `AC-3` registration can corroborate descriptor `0x6a`.

The DVB E-AC-3 carriage requires PMT stream type `0x06`, an elementary
`0x7a` descriptor and PES stream ID `0xbd`. A matching elementary
`EAC3` registration can corroborate the descriptor. Automatic probing
leaves the descriptor namespace unspecified. The reader admits one
independent substream (ID zero) followed by up to eight ordered dependent
syncframes at the same sample rate and block count. Converted independent
syncframes carry no dependents. Each group ends at
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
access-unit framers across work grants, counting each shared backing once. A
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
atomically with the corresponding source event. A PMT stream-list change keeps
framing for routes whose signalling and clock binding still apply. Published
packets use the active compatible PMT and retain their original PES ranges.
A PAT that invalidates the active PMT clears the catalogue. Routes affected
by a table, clock or interrupted-byte-run boundary report unpublished bytes
through `ReaderEvent::Interrupted`.
Logical EOF drains the framers before the original `Finished` event and final
`Step::End`. When a PES needs a larger header or payload source-range table,
the reader checks the allocations it must retain across that growth. If they
cannot fit together, it reports a PES `Limit` diagnostic. Memory held by caller
outputs can cause retryable `Backpressure`, with the original bytes and ranges
preserved on retry. If the pool cannot also hold a diagnostic for a permanent
PES limit, the reader returns the limit directly.

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
use triergon_mpegts::{PacketFormat, ParseLimits};
use triergon_mpegts::stream::{TraceEvent, TransportStream};

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
| Whole stream, one program | `stream::TransportStream::{new, new_with_namespace}` |
| One HLS segment (TS188, in-band PAT/PMT) | `TransportStream::new_single_program_segment`, `new_single_program_feed` |
| Sequential TS188 playback input | `TransportStream::new_single_program_progressive_feed` |
| Packets only | `transport::{parse_transport, TransportFramer, TransportInspector}` |
| Continuity | `transport::Continuity::observe` → `ContinuityOutcome` |
| PSI | `psi::{SectionAssembler, PatReader, PmtReader, PatCollector}` |
| Track classification | `psi::ElementaryStream` implements `triergon_core::container::TrackInfo` |
| Descriptors | `psi::Descriptor::{registration, languages, avc_video, hevc_video, dvb_ac3, dvb_eac3, dvb_subtitling}` |
| Stream codec | `psi::ElementaryStream::{codec, dvb_descriptor_codec}` → `triergon_core::CodecId` |
| PES | `pes::PesAssembler` → `Pes`, `PesHeader::{raw_pts_time, raw_dts_time}` |
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

The startup probe scores sync bytes in groups while charging each inspected
byte. Complete TS188 and M2TS192 carriers use one validator for common
payload and adaptation layouts, including PCR. M2TS prefixes stay opaque.
Program playback keeps tight carrier storage so a packet cannot retain an
entire input window. A complete buffered carrier can reserve its storage and
shared header together. The PES observer checks that the payload is the exact
span at its declared offset in that carrier. A ready PES continuation can
retain just the payload view while playback delivers the full transport event.
For a carrier that needs incremental assembly, the observer retains the
charged bytes and the fields used for packet order and offsets.

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
range with the same admission and retirement rules. `PayloadCursor::has_current`
reports whether the cursor has an active range. Full carrier facts remain available
through `current` and `next_span`. A borrowed span iterator can
advance through the same ranges with `DemuxSpanViews::next_bytes`, which returns
only the borrowed bytes.
A consumer that has used a packet up can hand its lists back:
`PayloadCursor::into_spans` empties the span list, and
`DemuxObserver::reuse_payload_spans` and `reuse_header_ranges` give the span and
header-range lists to the observer that emitted them, whose next PES then fills
them without allocating. A list is kept only while its capacity is at most twice
what a new list grew to for the last PES (`BudgetVec::fits_reuse_for`), and only
when the observer's own list is empty. Admission, payload runs and growth work
follow the capacity a new list would have; only the allocation is kept.
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
  `discontinuity()`. For legacy streams with incorrect PCR reserved bits,
  select `compat::pes::PcrPolicy::IgnoreReservedBits` in `ReaderLimits`.
  The clock still requires a valid extension and adaptation field.
  `Adaptation::pcr_reserved_bits()` exposes the encoded bits.
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
  ATSC/Blu-ray signalling, descrambling, multi-program PES output.

## Conformance

Specification basis:

- ITU-T H.222.0 (08/2023): §2.4.3.1–2.4.3.5 (transport packet, adaptation
  field), §2.4.3.6–2.4.3.8 (PES), §2.4.4 and §2.6 (PSI and descriptors),
  Annex A (CRC-32).
- ETSI EN 300 468: §6.2.41 / Table 99 (subtitling descriptor), Annex D.3 and
  D.5 (AC-3 and E-AC-3 descriptors).

Verification:

- The [DTS seek comparison](../../harness/features/mpegts-dts-seek/contract.json)
  contains two DTS core history cases and 24 authored XLL cases. Twelve XLL cases
  use an extension alone and twelve carry an independently recoverable core.
  The cases cover short extension headers in TS188 and long extension headers in M2TS192, both
  seek policies at 90,000 and 91,920 source ticks, explicit and missing source DTS,
  and an indexed backward seek after draining to EOF. Each suffix includes every
  payload byte and its source clocks. The fixture author is
  `cargo xtask fixture transport-dts-seek --record REFERENCE`; it records folds
  through the FFmpeg TS audio seek worker and reproduces the existing core cases.
  FFmpeg identifies the DVB extension descriptor as data and preserves the aligned
  full PES payload. These inputs contain only self-contained XLL entries with
  checked recovery navigation; the independent reader tests cover refusals.
  Synthetic bodies exercise framing and recovery metadata, not audio decoding.
  Run `CARGO_INCREMENTAL=0 cargo xtask build-workers --only transport-worker`, then
  `CARGO_INCREMENTAL=0 cargo xtask report --contract harness/features/mpegts-dts-seek/contract.json --manifest harness/features/mpegts-dts-seek/corpus.json --corpus . --output DIR`.
  Adding `--benchmark --memory --require-slice-acceptance` applies the complete
  per-case runtime, live-heap and total-RSS contract on each required native target.
  [Native results](../../docs/status.md) include both XLL profiles on AArch64 and ARMv7.
- `CARGO_INCREMENTAL=0 cargo test -p triergon-mpegts --test container_reader seek::dts_hd`
  checks DTS-HD seek metadata, full original packet and PES provenance, exact clocks,
  small input/work grants, TS188/M2TS192, previous/preroll landings, index reuse,
  tied timestamps and explicit refusals. Synthetic payloads test recovery syntax,
  not decoded audio.
- The ignored test `fate_lossless_only_entries_decode_the_exact_pcm_suffix` uses
  FATE `dts/dcadec-suite/xll_51_24_48_none.dtshd`, SHA-256
  `c445bb746c6afc920761b55b00932053d64a96d3d08cd9cad4c759a892fae4cd`.
  Set `TRIERGON_DTS_HD_XLL` to that local file and optionally `TRIERGON_FFMPEG`
  to a decoder binary, then run
  `CARGO_INCREMENTAL=0 cargo test -p triergon-mpegts --test container_reader fate_lossless_only_entries -- --ignored`.
  It checks every source field of three seek suffixes and compares their complete
  PCM bytes with sequential decoding through the FFmpeg command-line interface.
  The sample stays outside the repository. This is functional coverage, not
  runtime or memory qualification.

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
  through `transport-worker`'s `ts-` modes, which use the public API only:
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
  The 272 recorded requests map to 97 PES compatibility-view operations.
  All 97 match the pinned FFmpeg formats worker on AArch64 Linux and macOS.
  In the full gate runs at `1c64b775`, all 97 pass the runtime ceiling on
  Linux AArch64 and ARMv7 (highest upper bounds 0.7226 and 0.8548) and use
  less peak heap and total resident RSS. The strict source requests
  supply input for this compatibility comparison. Full source and evidence
  hashes are in [status](../../docs/status.md#measured-revisions).

- The [source polling](../../harness/features/ts-source-poll/contract.json),
  [standalone PES assembly](../../harness/features/ts-pes-assembler/contract.json)
  and [demux observer](../../harness/features/ts-observer-forms/contract.json)
  comparisons use every input from the PES packet corpus. They exercise five
  source poll forms, four payload chunk sizes and three observer forms. The
  strict source comparisons preserve encoded timestamp absence, period-zero
  ticks and physical carrier positions. The observer comparison uses projected
  demux clocks. The contracts list the outstanding differences and the
  runtime and memory bounds awaiting native measurements.

Gates, harness and current results: [docs/quality.md](../../docs/quality.md)
and [docs/status.md](../../docs/status.md).
