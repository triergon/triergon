# Typed playback composition (experimental)

Consumer-neutral source acquisition, bounded container events, packet/subtitle bridges and owned producer/pump handoff. This optional crate sits above the independent Matroska, MP4, TS, framing and subtitle leaves. Placing it in the Matroska crate would introduce upward dependencies on unrelated formats; the core remains format-neutral. There is no provider, HTTP client, native platform sink, rendering, watch-state or transport retry policy in this graph.

The Source protocol requires prompt polls and actual EOF replies to positive-size read requests; starvation and transport failures are separate. Source.abort wakes/quiesces a separate I/O producer before close. It must not join the current demux thread. The generic Sink owns accepted Feed envelopes and retains them through actual native/callback lifetimes; Blocked returns the same owner. Callback tokens, source generations, configuration revisions and decoder epochs remain distinct. Platform/main-thread restrictions remain the sink implementation's responsibility; no Send bound is added.

The selected functional profile includes real Matroska/MP4/TS reads, source/native views, configuration transitions, movie/source seek and preroll, queue pressure, pause/teardown and explicit source-entity continuation. Progressive front-moov MP4 reads later bounded moof/mdat batches without claiming an early EOF. Pending moofs allocate charged retained traf slots on demand, reusing completed holes, up to the configured maximum of 64. With no pending cursor, the continuation constructor makes no slot allocation. A growth refusal retains its completed cursor and leaves the next ordinal/end uncommitted. A per-sample merge runs at each actual MDAT in the selected Source or native order. Completed slots retire and future sample heads remain owned across later media extents; unknown fragment clocks stay unknown. The terminal-MDAT continuation adds a checked size-zero top-level declaration for SourcePhysical: front-movie startup or a later progressive media unit, with complete sample reads and bounded opaque-tail drain to an actual source/token End reply. Pending/empty input and end-of-index never fabricate EOF. The declaration retains its unknown endpoint. Initial startup after earlier consumed media remains refused; the default native terminal-context refusal, in-band movie catalogue updates and broader progressive variants remain separate. The additive native profile exposes `Mp4IndexJob::new_native_terminal_v1` and `Engine::mp4_native_terminal_v1` for an explicitly selected from-start nonseekable native view. Its derived native predecessor clocks never fill Source timestamps. A typed zero-read boundary can end native packet delivery at the first sample of the first fragment run or at the last sample admitted in the current media extent. Source reads continue through later media extents, and the operation completes on the matching Source End. For the single raw-video track profile without composition offsets, an interior zero can also leave unassociated native empty packets for the remaining slots in its run. Those packets have their own event type and carry no Source sample. The last uses the greatest completed run end or declared media duration. Other interior zero shapes remain explicit gaps, with their original native captures retained. Opener budget Backpressure retains the same source and consumed metadata/reference/fragment owners for retry. Hard syntax, policy, transport and cancellation failures still retire that opener. Native M2TS startup uses only the existing finite aligned single-window context and a separate actual EOF read; short/fragmented/mixed/long or unproven contexts remain refused by that profile. Ordinary strict M2TS and the old default native refusal remain separate.

MP4 catalogue replacement is an explicit fresh-entity transaction. The caller independently opens/indexes a stable source with a distinct SourceId and chooses its existing source/native profile. Preparation validates pool, source binding, selection and delivery epoch and reserves only the actual retired FIFO slots; work zero makes no progress. Refusal, dropped preparation and cancellation of this replacement attempt leave the active source, cursors, queue and candidate alive for retry. Commit aborts/quiesces/closes the old transport exactly once and returns its retained engine, FIFO, pending event and request as retired readable facts. Raw track IDs, description bytes, source clocks and cursor ordinals are not remapped. The new engine emits its own configuration before packets. Pause and the cumulative event cap survive; the old seek intent belongs to the retirement receipt rather than the new clock. Regular player cancellation still tears down the player. This does not accept an in-band second moov or establish a native catalogue-replacement operation.

Pump replacement requires blocked and accepted Feed envelopes to retire first. A separate CatalogueSink two-phase boundary prepares fallible sink resources before the player commits, abandons preparation on refusal/cancellation, and commits without allocating or losing callback owners. That boundary preserves pause and installs a fresh callback fence. Actual platform decoder configuration/flush/thread scheduling remain the injected sink's responsibility.

HEVC/AVC framing and AAC ADTS projections preserve raw packet/config/timing facts. AAC configuration association is supplied explicitly by the caller; this does not prove an opaque source configuration belongs to a hardware decoder. Text packets retain source bytes, including UTF-8 continuation bytes, and exact/unknown source clocks. Matroska text handoff uses a bounded charged copy where the frozen media API cannot alias a core Bytes owner. It is not a memory improvement claim. Authored ASS remains opaque. Bitmap jobs keep source planes, palette/geometry facts, all DVD units and typed tails distinct from explicit native receiver views and omissions.

The experimental Root3477 composition includes this crate and its additive terminal-MDAT profile. Native Linux ARM64 functional gates record 3,477 passed, zero failed and one ignored test in both debug and release. The separate terminal-MDAT run matched all 120 required whole native observations across five fixtures. Its wider matrix retains 746 native matches and 252 nonpasses. Adoption does not establish full enabled-feature/native parity or numerical qualification. The remaining codecs/streaming variants, actual Streem/native Load/A-V/presentation wiring, simulator/TV and numerical gates remain required. The historical prototype and all its source/raw/RED/gold identities remain immutable. Later integration changes require exact preimage checks and fresh affected gates.

## Matroska AVC and AAC feed

Prepare `MatroskaFeedSetup` from the Player configuration event and one
selected track. Setup shares the charged initialization record from the
active TrackEntry or CodecState with the framing parser. The AVC view
supplies a four-byte Annex-B start code per NAL and prepends the current
SPS/PPS before an IDR access unit. The AAC
view supplies a seven-byte ADTS header using the indexed AAC LC core
configuration. The output borrows the original packet bytes and preserves
the original timing and packet facts.

Submit outputs only while `Player::may_deliver` authorizes them. The feed
leaves unselected packets on the source path and rejects an incompatible
track, configuration revision or Matroska generation. Rebind after seek
or CodecState changes. Malformed AVC tails fail before publication.
The selected AAC feed requires an explicit indexed channel configuration.

Use `Engine::matroska_with_payload_ownership` with `SharedBounded` when
complete blocks can borrow an immutable source window. A borrowed packet
retains the full backing allocation and its budget charge until the last
alias drops. A block split across reads uses the bounded copy path.
`Engine::matroska` uses exact-size packet backing.

The three-track fixture verifies complete packet order, feed bytes,
FFmpeg clock projection and retained source clocks at two read sizes.
It also verifies pause, seek and audio-track switching in the Player.
The five selected native Linux ARM64 cases pass parity, the 1.25×
delivery ceiling, live heap and total RSS with shared blocks and a
borrowed direct output slot. Streem caller wiring and the 0.80×
optimization target remain open.
[Matroska codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html)

## AV1 temporal-unit delivery

`Av1BridgeSetup` binds the current Matroska track initialization or MP4
`av1C` record to the typed packet configuration. A generation or revision
change on Matroska requires a new bridge. On MP4, a seek can reuse the
bridge while its sample description remains current. Feed the output
segments in order. They borrow the original sample and any inserted
configuration, so the caller can gather them into its native feed buffer
without first copying the compressed packet into intermediate storage.

The selected view inserts an empty delimiter when absent, supplies a
configuration sequence header and metadata at random access when the
sample omits its sequence header, and writes a size for a final unsized
OBU using the complete sample boundary. Source bytes and clocks remain
available through the original packet. The host regression matches all
six frames from both MP4 and WebM at two input read sizes to FFmpeg's
delimiter-insertion output. Decoder behavior and numerical qualification
for this view are open. [AV1 sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format)

## Playback format selection

`io::PlaybackProbe` accepts a single caller-owned source and selects
Matroska, MP4, TS188, M2TS192, AVC Annex B or HEVC Annex B. It reuses the
container signatures from `io::Probe`. Matroska needs four probe bytes,
MP4 needs eight and the transport signatures use 389. If those signatures
are absent, the bounded raw scan selects from the same retained prefix.
A conflicting transport signature is an error.

The selected container has the usual `SelectedInput` for the existing
openers. A raw selection has a `RawVideoSource`. Both consume the
retained prefix and continue at the first unread offset. For a short
raw source, the handoff also preserves confirmed EOF. The older
container-only and raw-only probes remain available.

## Raw AVC and HEVC input

Use `raw_video::RawVideoProbe` when the input is raw Annex B and the
codec is unknown. It bounds the byte and NAL scan, requires ordered
parameter-set signatures followed by a compatible slice prefix, and
replays the whole accepted prefix through `RawVideoSource`. The
underlying sequential source continues at its next offset. For an
already identified codec, construct `RawVideoSource` directly.

`RawVideoConfigTracker` resolves the active parameter sets referenced by
the first coded slice in each access unit. The result retains the original
SPS/PPS and optional HEVC VPS owners and reports the cropped display
raster. Parameter-set updates affect subsequent pictures while previous
results keep their bytes. AVC configuration also exposes
`num_units_in_tick`, `time_scale` and `fixed_frame_rate_flag` when the
active SPS carries timing information. The caller supplies per-picture
timing and the sink. Pending and empty responses preserve the request.
Confirmed EOF releases the last packet. Transport failure and
cancellation close the source.

An active HEVC configuration can independently project its SPS VUI clock
and any proportional-to-picture-order-count fields through
`RawVideoConfiguration::hevc_timing`. The raster path remains usable on
the basis of the SPS prefix. The timing projection covers the admitted
base-layer SPS grammar.

For a sink-ready path, call `RawVideoSource::into_feed` before the first
access unit. `RawVideoFeed` combines the original Annex B spans with the
active configuration and a revision that changes only when the active
parameter sets change. Retryable configuration pressure retains the
pending packet and clock. An unspecified submission clock maps to zero
only at the queue boundary. A caller-supplied rational submission
cadence advances once per coded picture, independent of SPS timing.

The host comparison covers eight complete streams and all 291 packet
positions, lengths and key flags, plus the initial active parameter-set
chain, cropped raster and literal VUI fields. The reference has absent
PTS and DTS on these inputs. A separate fixture covers AVC resolution
changes and exact submission pacing. Per-picture presentation order
and native numerical qualification remain open.

## Event delivery

`Player::advance_ready` publishes a ready event in the current call if
the FIFO admits it. It uses the same byte, slot, work and event limits
as `advance`. The pump uses this entry point. Use `Player::read_output`
with one empty caller-owned slot for immediate consumption. It drains an
older FIFO entry first and reports `Progress::Queued` when it fills the
slot. A pending source request keeps its token across calls. An occupied
slot reports backpressure. `Player::new_direct` starts without reserving
FIFO slots for a caller that uses `read_output`. Its first FIFO advance
reserves the configured slots and can report retryable pool pressure.

## Range-backed MP4 sync seek

For a completed range-backed SourcePhysical plan, prepare a seek with
`Player::prepare_mp4_sync_seek`, poll its bounded row scan and apply the
command with `Player::apply_mp4_sync_seek`. The target is exact source
presentation time. A caller with movie time can first locate the source point
using `PreparedMp4::movie_seek`. The planner uses the presentation
predecessor and the preceding container sync in its decode-order
description run. Earlier selected subtitle or secondary-video state can
move the first range backward. Known audio decode extents preserve the
caller-supplied preroll. The command is bound to the current player
generation, and a successful apply republishes track configuration.
[QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file)

The encoded AVC/AAC regression checks a later sync, interleaved AAC
preroll, all bytes and timestamps in the selected suffix and the first
actual range request.

For complete range-backed `PinnedMov9NoParseNoFillin` playback with
`ExplicitNativeProjection`, keep the `Movie` from the same open transaction.
Call `Player::prepare_mp4_native_seek` with signed ticks in the selected
video track's native timebase. Poll the job, finish it and apply its
generation-bound command with `Player::apply_mp4_native_seek`. The planner
uses the pinned MOV index and rescales the selected video anchor into each
other track's native timebase, rounding ties away from zero. The engine
resumes each track at its reference anchor and merges those heads in
reference order, even when the next range moves backward. Planning reads no payload. The first actual media request
starts at the command's `start_offset()`, and the engine republishes
configuration before delivering a packet that uses it.

`Mp4RangeOpen` offers the same native order and range seek over the borrowed
complete `Movie` and any checked encoded edit plans. The open job validates
source extents and native occurrences, then rewinds the checked tables before
reading payload. It also derives the duration when the movie header omits it.
The direct-output player keeps the compact tables and per-track cursor state
and constructs packet locations and clocks as they are used. For regular
tracks with strictly increasing native clocks, seeking can resume from
charged, sparse checkpoints collected during preflight. The full seek planner
still handles other supported queries over the checked tables.

Five FFprobe replays compare the full post-seek sequence for regular,
fragmented, repeated-edit, description-change and mixed-clock AVC/AAC
files. The profile requires a supported index clock query.

For a range source with front metadata, `Mp4Open::new_progressive` can
start playback at the first media header. To seek, independently open and
index the same stable byte entity with `Mp4Open::new`, using the same
`SourceId` and a separate transport owner. Prepare the command with
`Player::prepare_mp4_progressive_sync_seek` and apply it through the player
or pump. Until commit, the progressive source, queued output and candidate
remain available. Commit retires the old transport, preserves pause and
starts the new generation at the checked sync/preroll range. The pump
drains outstanding sink ownership before switching its callback fence.

The explicit progressive native clock can use the same transaction. Build
a complete pinned native range candidate for the same entity and keep its
`Movie` with `Mp4Catalogue::new_with_movie`. Prepare with
`Player::prepare_mp4_progressive_native_seek` and apply through the player
or pump. The fragmented replay matches six pinned FFprobe packets with
their bytes, clocks and flags. The sink receives the new generation at
commit and playback stays paused until resume.

Overlapping native clock queries, recovery range seek, numerical
qualification and application/device verification remain open.

## Native terminal MP4 catalogue replacement

For an explicitly selected nonseekable native profile, build the index with
`Mp4IndexJob::new_native_terminal_v1` and attach the replacement using
`Mp4CatalogueMode::NativeTerminalV1`. The replacement follows the same
front-metadata and source EOF checks as the standalone player.

## Native PGS playback

Choose `PgsMode::NativeCompatibility` for the native receiver's first public
event. `PgsStep::Compatibility` carries the original charged packet owner and
the native decoder output through the same generation and configuration fences
as the other PGS modes. `PgsCompatibilityOutput::packet_clock_action` projects
the first native event into a borrowed indexed picture or a clear, using
the existing bitmap queue's packet clock. An absent event leaves the queue
unchanged.

## Matroska video packet clocks

Create `native_avc_clock` or `native_hevc_clock` from the selected feed
and observe each checked packet once in input order. The clock uses the
active parameter-set chain's reorder depth and keeps early DTS unknown
until its window has enough presentation timestamps. It accepts one
progressive AVC picture or one base-layer HEVC picture per packet,
starting at a random-access picture. A playback scope change invalidates
the clock.

After a seek, bind `MatroskaVideoSeekGate` to the player's seek intent and
resolved Matroska receipt. Send its admitted video packets to a fresh feed
and clock. The source can replay packets from earlier in the landing
cluster. The gate starts the selected video at the resolved keyframe and
preserves the player's decode-only classification up to the requested
presentation time. Other consumers can still use the complete source
outputs.

For an immutable `ByteSource`, `Engine::matroska_with_immutable_seek_index`
can reuse validated Cluster positions after a complete sequential read. It
keeps a bounded, budgeted index of keyframe timestamps and requires each
supplied range to be a view of the same source bytes. Contiguous packet
payloads can share that charged allocation. A complete index can serve
playback seeks with no Cues and no codec-state or subtitle recovery. Other
cases use the normal validated seek.

The native clock keeps any duration supplied by Matroska. For HEVC with
no duration on a millisecond or finer packet grid, it uses the VPS clock,
then SPS VUI timing if the VPS has no clock, and rounds down to whole
packet ticks. The original container packet still carries its source
clocks.

## Matroska AAC packet clocks

Create `native_aac_clock_probe` from an admitted AAC feed and retain
the caller's selected packets through discovery. Push each packet once,
then `finish` and replay its timing by ordinal. `into_live` continues
that clock after releasing the probe rows. The bounded projector uses the
declared duration when available. Otherwise, it projects the admitted
1024-sample AAC-LC core period onto whole Matroska ticks. A new seek
generation needs a fresh feed and clock. The original packet retains its
source clocks and bytes.

The mixed-audio regression checks all thirteen AAC packets at 48 kHz and
44.1 kHz against pinned FFprobe, including negative startup time, DTS and
duration. [Authored fixture](tests/fixtures/mkv-feed/README.md)

## Matroska Dolby audio packet clocks

`MatroskaHevcDolbyPacket::native_duration_ns` returns the declared
duration on the native packet clock. When that duration is absent, the
method checks up to 32 complete independent AC-3 or E-AC-3 syncframes.
It checks the final frame’s CRC and projects that frame’s coded sample
span down to a whole Matroska clock tick. Source timing remains available
on the packet.

For native-compatible PTS and DTS, create a `native_dolby_clock_probe`
from the selected feed. Push the selected, checked packets once in input
order through the caller's discovery cutoff. The probe retains up to 4096
budgeted timing rows. An absolute timestamp within that prefix can anchor
earlier packets, and a later coded duration can fill eligible leading
rows. Call `finish` and replay the prefix timings by ordinal. Use
`into_live` to release the rows and advance the same clock as more
packets arrive. Rebuild the feed and clock after seek or configuration
change, and keep the player's delivery classification when replaying the
original packets. The source-clock methods `push_source` and
`observe_source` accept the original selected demux packet. They let the
caller preserve packet bytes and timing even when the syncframe job
refuses a damaged packet. A missing AC-3 duration can use its declared
rate and fixed 1536-sample period when that period is a whole number of
Matroska ticks. The decoder still checks frame admission separately.
See the [timing controls](tests/fixtures/dolby-clock/README.md).

## Matroska bitmap subtitles

Bind `matroska_feed::MatroskaBitmapTrack` to the selected TrackNumber using a
current player configuration or packet output. Pass its scope to `PgsBridge`
or `DvdReceiver`. For VobSub, poll `vobsub_configuration` for the effective
palette and canvas first. Pass only outputs accepted by `Player::may_deliver`.
For DVB, open a configured `dvb::Decoder` with `dvb_configuration` and the
binding's generation, then supply the bare segments returned by `dvb_packet`.
`MatroskaDvbPacket::packet_clock_action` uses the native publication triggered
by that packet and returns the queue's borrowed picture or clear.

The bridge shares the demuxer's charged, restored frame bytes, including frames
with supported content compression. The decoder input also retains the typed
source packet. The queue uses the pinned Matroska packet clock, while the typed
packet keeps exact source time and BlockGroup metadata. The compatibility
clock precedes FFmpeg's generic stream-probe timestamp and duration fill-in.
After a seek or configuration change, bind to the new output and reset the
receiver with the new scope. The packet metadata retains the delivery
classification so callers can decode preroll without presenting it.

Use `MatroskaDiscovery` to gather an initial prefix on the player's Source.
It observes PGS canvas packets, recognizes admitted subtitle declarations,
and checks for a timed, nonempty packet on admitted AAC-LC, AC-3 and E-AC-3
tracks. AAC-LC uses bounded configuration parsing. AC-3 and E-AC-3 use the
declared audio format or a bounded syncframe header. Video discovery can
also observe timed AVC and HEVC pictures with one verified parameter-set
chain and a declared frame period. AVC uses progressive pictures and the
signalled reorder depth or Constrained Baseline rules. HEVC uses a
single temporal sublayer and its SPS reorder depth. The observation keeps
packets in input order. Consumers can report other readiness against the
current configuration using `confirm_stream_ready`.

Discovery stops when all streams are ready, a packet, transport or output
limit is reached, or the source ends. The same player resumes after
`finish`, including when a transport cap lands partway through an element.

`MatroskaCompactDiscovery` uses the same cutoff and source continuation. Its
prefix exposes typed events for inspection and moves the original scoped
outputs out in order via `into_outputs`. The full-output prefix is also
available through `MatroskaDiscovery` for consumers that need a borrowed
output slice, such as the bitmap clock probe below.

Give the returned ordered prefix and its output cap to
`MatroskaBitmapClockProbe` for the selected track. The clock projector
also accepts a prefix buffered by an existing caller.
Poll the clock projection before replay, then use `packet` or `dvb_packet`
with the original output index. This can backfill a leading unknown clock
and duration using later buffered timing, including the first absolute
anchor. Payloads and source timestamps remain shared. Once the prefix is
delivered, `into_live` releases the borrowed prefix and carries the clock
forward. Keep a returned input for receiver backpressure retries. Use the
player's delivery and generation checks when replaying buffered outputs.

## Traditional DVD subtitles

Use `DvdJob` with `DvdMode::ExplicitNativeFirstUnit` and call
`DvdOutput::native_presentation` for the borrowed bitmap and the show and
optional clear boundaries. Absolute times use the packet PTS. Source
control dates and command order remain available through each picture's
program. Use `DvdReceiver` for a sequence of native first-unit packets.
It carries the completed packet's control state to the next packet and
retains a pending job through pool pressure. Reset with a new playback
scope after a seek or track/configuration change. The caller owns
scheduling across packets. After consuming an output, the caller can return
it through `DvdReceiver::recycle` to reuse its small charged tables on the
next packet. For the existing packet-clock bitmap queue, use
`DvdOutput::packet_clock_picture`. It borrows the same indexed bitmap, filters
the configured canvas to the queue's supported dimensions, and uses packet
PTS, then DTS, then zero. A stop-only packet without a bitmap leaves that
queue unchanged. The explicit DVD show and clear boundaries remain available
through `native_presentation`.

## DVB subtitles from TS

Use `Engine::ts_with_namespace` with the explicit DVB namespace. Create a
`dvb_ts::DvbTsBridge` from the active program-map output for the chosen PID.
`new_for_service` selects a descriptor and service entry in wire order
and retains the full selected descriptor. The decoder uses that entry's
composition and ancillary pages. The single-service `new` constructor
accepts one subtitling service. Push strict PES outputs, poll through
display events, then finish and drain at source end. An unsplit PES shares its original storage.
A split PES is gathered incrementally into one charged buffer. The decoder
retains the original PES data field, its descriptor, and the available
source clock. Recreate the bridge on a new player generation, configuration
epoch or transport clock epoch.

Use `new_with_native_pes_parser` when the caller needs the selected native
codec packet boundary. A changed known PTS starts a new group. Complete
segments from one PES borrow that allocation. Continued segments use a
charged buffer capped at 65,536 bytes. The parser keeps that buffer across
incomplete fragments and publishes it when a segment completes. The
display event retains both the
original PES facts and the projected codec packet. A short or invalid codec
header is reported before decoder admission. Read its retained bytes with
`take_rejected_native_pes` and continue with the next PES. The native clock
can inherit the group's PTS while the source PTS remains unknown. These
paths have selected packet and bitmap tests. Full DVB and numerical
qualification remain open.

With `ReceiverView::NativeD32`, use `packet_clock_action` to translate each
event into the bitmap queue's clear or show action. It borrows the indexed
rectangles and native palettes in receiver order. The queue timestamp comes
from the packet that published the event, using its projected PTS when
available, then DTS, then zero.
The canvas is present when the decoder established one within the queue's
size limits. Streem scheduling, multi-service native comparison and
numerical qualification remain open.
[ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf)

## Native fragment header admission

The pinned native opener rejects a zero-size sample in a fragment reached
during header parsing. A seekable open scans the complete file, and a
forward open checks the fragments before the first media extent. Later
forward fragments still use the packet-stage zero-size behavior. The
Source opener preserves the original sample declarations in either case.

## Native short packets

`Mp4Open::new_pinned_mov_recovery` selects the native short-packet view
for finite media and progressive terminal media.
At a matching physical EOF, a sample with a positive incomplete prefix
produces `Mp4Event::NativeShortPacket` with the corrupt flag. The event
retains the original sample declaration, actual bytes and EOF coordinate
separately. Pending input, transport failures and an empty prefix still
follow their own outcomes. SourcePhysical uses the strict opener and
complete sample reader.

## Invalid regular MP4 metadata

The pinned recovery view of an unedited regular raw-video track can project
a zero media timescale as a native `1/1` clock. The original `mdhd` is
retained, with an unavailable Source timebase. Source-clock seeking still
requires a valid declaration.

For an unedited single-track regular file, a failed layout check can lead
to a separate native read refusal. The engine emits `NativeMissingSample`
only when the first declared packet starts at or beyond the confirmed
physical end. It emits no media packet for that range.

## Reversed fragment offsets


For a complete index, the playback planner validates every sample extent
before sorting a nonmonotone physical layout in place. The Source view
retains physical byte order, with ties ordered by track ID and source ordinal
on this fallback. The native view still schedules from each track's next
decode occurrence.

On a forward-only input, the native fragment scheduler selects from each
track's next source ordinal. If that requires revisiting earlier bytes in
the current MDAT, it reads one complete finite extent of up to 32 KiB
and shares its retained bytes across packets. The Source view preserves
physical order. Backward native reads across extents or outside this
cache bound remain unsupported.

## Forward regular MP4

On a forward-only source with media before `moov`, the opener can retain
whole finite MDAT payloads up to a combined 32 KiB. Packets borrow shared
slices of those payloads at the offsets validated by the index. The opener
still waits for actual source End when it scans the complete entity. A
sample whose bytes passed outside the retained extents remains unsupported.

When a front-metadata regular movie's edit list requires a different native
packet order, the explicit forward native view can retain its single finite
MDAT within the same 32 KiB limit. Repeated packet occurrences share slices
of that extent and keep their original source sample coordinates. The Source
view still enumerates the physical samples once. Fragmented movies and media
outside this bound require another source context for reordering.

For an unedited regular movie with metadata before media, the forward reader
can continue across finite MDATs into an MDAT declared to extend to EOF. It
validates each pending sample against the actual media declaration and reads
its complete payload. A terminal MDAT finishes on the matching Source End.
Backward per-track chunk order is unsupported. Samples in a verified gap or
crossing a finite extent fail containment.


## Native TS188 private audio

Select `Engine::ts_native_private_audio_v1` with the PID and one of the
four observed AC-3/E-AC-3 carriage profiles. The active PMT's program and
elementary descriptor loops feed the bounded TS observer. A mismatch emits a
native refusal and preserves the original Source packet. Private stream type
`0x06` becomes selectable as audio only after that explicit admission.

The Source clock view continues to deliver Source packets. In the explicit
native projection, the selected PID's Source PES remains available as facts
and its native PES supplies delivery. Other PIDs retain their usual delivery
classification. The packet-level contract covers finite TS188 PES; decoder
access units and application-level integration have separate gates.

## Multiple native TS188 tracks

`Engine::ts_native_program_v1` selects up to 64 distinct PIDs in one
program. Registered, private audio and explicitly declared DVB subtitle
carriage share the same program clock. Each PID owns its own assembly state.
An active PMT validates one selected PID per work step. A carriage mismatch
emits a refusal for that PID after the PMT event, while the other PIDs and the
raw Source stream continue.

Completed packets follow carrier order. Any packets left at logical EOF
drain by ascending PID, matching the observed no-parser FFmpeg controls.
Source PES timestamps keep their original presence and values. The caller
declares the DVB descriptor namespace. Allocation backpressure retains the
pending carrier and resumes the same observer when the pool has space.

Select `TsNativeCarriage::AudioFromPmt` to follow audio carriage changes
on a selected PID. This profile resolves ADTS, LATM and the established
private AC-3 and E-AC-3 declarations. Stream type `0x06` requires explicit
DVB context. Each admitted PMT emits a Source configuration event with the
resolved carriage and epoch before that epoch's native packets. A refusal
keeps the original Source PES available. A seek starts a fresh projection
and announces configurations from the replayed PMTs.

## Bounded M2TS prefix startup

Use `NativeStartup::new_m2ts_prefix_v2` for an initially selected M2TS192
Source with an input window that can hold 8 KiB. The operation assembles a
complete prefix from actual replies and requires a decisive physical grid.
It hands that prefix to the same cursor and Source for playback. A complete
single reply is shared directly. Smaller replies are assembled within the
shared budget and a completed reply survives allocation pressure.

Playback uses carrier-start native positions for this profile and reads
past the prefix until an actual Source End. Losing the physical grid
produces a typed native refusal while the strict Source events remain
available. A seek requires fresh startup evidence. The finite startup
entry point retains its complete-window and actual EOF requirements.
