# triergon-playback

Composes the Matroska, MP4 and MPEG-TS readers into a player-facing packet
feed: caller-owned sources, format probing, bounded container events, track
selection, seeking, and a producer/sink pump. It sits above
`triergon-matroska`, `triergon-isobmff`, `triergon-mpegts` and `triergon-framing`. It does not fetch over HTTP,
parse playlists, retry transports, decode media or render.

The MP4 range comparison includes two upstream samples, one with an initial
non-sync AVC prefix and one with tracks shorter than the declared movie.
It checks startup and backward seek through EOF for both.

## Usage

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{PacketFormat, ParseLimits, clock::SourceTimePolicy};
use triergon_playback::{
    ByteSource, Engine, Player, PlayerPolicy, Progress, Selection, io::SourceOwner,
};

# let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hls/media-00.ts");
let pool = Budget::new(64 << 20)?;
let id = SourceId(1);
let bytes = Bytes::copy_from_slice(&pool, &std::fs::read(path)?)?;
let caps = SourceCapabilities { length: Some(bytes.len() as u64), random_access: true };
let source = SourceOwner::new(ByteSource::new(id, bytes, 64 * 1024)?); // or your own `Source`
let input = InputCursor::new(&pool, id, caps, 4096)?;
let (program, policy) = (1, SourceTimePolicy::RelativeEpochZero);
let engine = Engine::ts(input, PacketFormat::Ts188, program, ParseLimits::default(), policy)?;
let mut player = Player::new(engine, source, Selection::All, PlayerPolicy::default())?;
let cancel = AtomicBool::new(false);
let mut packets = 0;
loop {
    let progress = player.advance(&mut WorkBudget::new(4096), &cancel)?;
    while let Some(out) = player.pop() {
        if player.may_deliver(&out) && out.event().track().is_some() {
            packets += 1; // deliver(out.event())
        }
    }
    match progress {
        Progress::Ended => break,
        Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
        other => return Err(format!("{other:?}").into()), // Backpressure, Paused, AwaitEntity, ...
    }
}
assert!(packets > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Implement `Source` for real transports. `ByteSource` serves immutable local
bytes. Use `io::Probe` when the container is unknown; its `SelectedInput`
replays the probed prefix and then continues reading. The probe accepts the MP4
opening boxes `ftyp`, `styp`, `moov`, `free`, `wide` and `mdat`. For transport
streams it examines a 389-byte prefix and requires exactly one of the TS188 and
M2TS192 three-sync patterns to match. Raw AVC and HEVC Annex B input is
[`triergon-raw-video`](../raw-video/README.md), whose `PlaybackProbe` tells it
apart from these containers.
MP4 needs an open and index step first: `mp4_open::Mp4Open` →
`mp4_plan::Mp4IndexJob` → `Engine::mp4`.
Indexed MP4 packets share immutable track metadata. A complete sample in a
shared input window can retain that window as its payload.
When the movie declares its duration, the range player checks source ranges and
packet projections before merging the tracks for delivery.
Regular AVC with a leading empty edit keeps its video start delay in range and
forward playback. A media edit can start at a later sync sample, and a backward
seek accounts for the composition lead. A short edit retains enough of the next
decode interval to complete its displayed pictures. An empty edit does not
produce a packet.
QuickTime range playback reads AAC configuration from version 1 and version 2
sound descriptions with `wave` extensions. The `avc-aac-quicktime-surround.mov`
fixture contains synthetic AVC and six-channel, 48 kHz AAC. The range comparison
also runs with the optional format and marker atoms replaced by free boxes.
`avc-aac-quicktime-reordered.mov` adds 44.1 kHz AAC and AVC with B pictures.
`avc-aac-quicktime-fragmented.mov` exercises open and seek with empty
initialization sample tables and a version 1 sound description. It remuxes the
synthetic `fragmented-avc-aac.mp4` fixture under the same MIT or Apache-2.0 license.
`avc-aac-quicktime-7-1.mov` uses the same synthetic video pattern and distinct
tones at 251, 379, 503, 71, 641, 769, 911 and 1031 Hz for the eight speakers.
FFmpeg 9.0.2 encoded the fixture as 44.1 kHz AAC-LC with channel configuration
12. The fixture is distributed under the repository's dual licence. The
[range corpus](../../harness/features/mp4-range/corpus.json) records its SHA-256.

`avc-aac-program.mov` uses an authored AVC pattern and seven independent tones
in a 48 kHz AAC-LC program with a leading edit. The range corpus compares its
original configuration and every selected packet on open and backward seek.
The fixture uses the repository's dual licence. To regenerate it with FFmpeg
9.0.2 and libx264:

```sh
signal='0.13*sin(2*PI*107*t)|0.13*sin(2*PI*211*t)|0.13*sin(2*PI*307*t)|0.13*sin(2*PI*53*t)|0.13*sin(2*PI*409*t)|0.13*sin(2*PI*509*t)|0.13*sin(2*PI*601*t)'
ffmpeg -nostdin -f lavfi -i 'testsrc2=s=96x64:r=25' \
  -f lavfi -i "aevalsrc=${signal}:s=48000:c=6.1(back)" -t 2.4 \
  -c:v libx264 -threads:v 1 -g 25 -keyint_min 25 -sc_threshold 0 -bf 0 -pix_fmt yuv420p \
  -c:a aac -aac_pce 1 -threads:a 1 -b:a 224k -flags:a +bitexact -fflags +bitexact \
  -video_track_timescale 16000 -movie_timescale 48000 -use_editlist 1 -movflags +faststart \
  avc-aac-program.mov
```

`Mp4RangeOpen::for_media` projects media tracks for playback and seek alignment.
Data tracks such as RTP hints and timecode still have their sample extents checked.
Supply any edit plans in source track order, with `None` for each data track.
The `avc-aac-rtp-hints.mp4` test fixture contains a synthetic AVC test pattern,
AAC sine audio and two RTP hint tracks, with the first hint track between the
video and audio tracks.

## Supported

| Area | Entry points |
|---|---|
| Source protocol | `Source`, `SourceReply`, `ByteSource`, `io::SourceOwner` |
| Probe | `io::Probe`: Matroska, MP4, TS188 and M2TS192 |
| Matroska | `Engine::matroska`, `Engine::matroska_reader` (an unstarted reader built with `Reader::new_with_payload_ownership`, `new_with_immutable_seek_index` or `compat::block_additions::new_reader`) |
| MP4 / MOV | `mp4_open::Mp4Open` (`new`, `new_progressive`, `new_recovery`, `with_timing_policy`), `Engine::mp4`, `Engine::mp4_progressive`, `Engine::mp4_native_terminal_v1`, `Mp4RangeOpen` / `Mp4RangePlayer` |
| MP4 seek | `Player::prepare_mp4_sync_seek` / `apply_mp4_sync_seek`, `prepare_mp4_demux_seek` / `apply_mp4_demux_seek`, progressive variants, `PreparedMp4::movie_seek` |
| MP4 catalogue replacement | `catalogue::Mp4Catalogue`, `Player::prepare_mp4_catalogue` / `replace_mp4_catalogue` |
| TS / M2TS | `Engine::ts`, `ts_with_namespace`, `ts_segmented`, `ts_native`, `compat::pes::open_private_audio`, `ts_native_program_v1`, `ts_native_segment_v1`; `compat::pes::startup::Startup` |
| PES feed | `pes_feed`: the feed [`triergon-hls`](../hls/README.md) reads HLS segments and progressive TS188 input from |
| Sink handoff | `pump::{Pump, Sink, CatalogueSink, Feed}` |

Matroska feeds, packet bridges and subtitle receivers over these outputs are in
[`triergon-bridge`](../bridge/README.md).

## Behaviour and limits

**Source protocol.** `Source::poll` must return promptly. `Pending` and empty
data never mean end of stream; only `SourceReply::End` does, and transport
failure is a separate reply. `abort` wakes and quiesces any I/O producer
before `close`. HTTP, authentication and retry policy belong to the caller.

**Bounded work.** Every call takes a `WorkBudget` and a cancel flag, and all
retained memory is charged to a `triergon_core::Budget`. `Backpressure` is
retryable and keeps the pending request, packet and buffers. `PlayerPolicy`
defaults to 16 FIFO slots (at most 64), 32 MiB queued payload, 128 MiB seek
input and 10,000,000 events. `Player::new_direct` skips the FIFO for callers
that use `Player::read_output` with one caller-owned slot. The player matches
each engine step where the engine wrote it, so a step without an event moves no
event-sized value (an MP4 packet event is 640 bytes on 32-bit ARM).
Transport carrier, section and PES outputs use the budget of their retained
storage for pool identity.

**Clocks.** Every packet keeps its exact source timestamps. FFmpeg-compatible
packet clocks are explicit opt-in views (`ClockChoice::Demux`,
`Mp4View::PinnedMov9NoParseNoFillin`, the `native_*` Matroska clocks and the
`*_native_*` TS engines); they never overwrite source facts. Native views can
emit events the source view does not, such as `Mp4Event::ShortPacket`,
`EmptyPacket`, `MissingSample` and native refusals.

**Generations.** A seek, track switch or configuration change starts a new
generation. Check outputs with `Player::may_deliver`. `Delivery::DecodeOnly`
marks preroll before the seek target.

**Matroska.** `SharedBounded` payload ownership lets packets borrow complete
blocks from the source window; a borrowed packet keeps the whole backing
allocation charged until it drops. Blocks split across reads are copied. The
packet's configuration supplies its allocation budget to the output.
The immutable seek index reuses keyframe positions after one sequential read and
needs no Cues.

**MP4.** Progressive playback of a front-`moov` file continues through later
`moof`/`mdat` pairs with at most 64 pending fragment slots. A size-zero
terminal `mdat` ends only on a matching `SourceReply::End`. Forward-only
reordering retains at most 32 KiB of media. An in-band second `moov` is not
accepted; replace the catalogue explicitly with a freshly opened source.
Range seeks plan from the sample tables without reading payload; the first
media request starts at the command's `start_offset()`.
When the movie header's duration is unknown or exceeds the presentation clock,
range playback finds the latest packet end while checking projections. It scans
in merged order when the seek restarts could reach their count or storage limit.
Fragmented files support an open-ended rate-one media edit. Forward playback
applies it from the front movie, including tracks first seen in later fragments. A
leading empty subtitle edit delays the subtitle packet clocks. When video
seeks before the first sync packet of a secondary fragment track, range
playback replays that track from its first packet. An AAC track with a
single initialization edit carries leading trim in the first packet's
`Mp4Packet::encoded_skip_samples`. A recovered short first packet carries its
leading trim in `Mp4ShortPacket::encoded_skip_samples`. The MPEG-4 object
type identifies AAC. An edited MP3 track has the projected clocks without AAC
trim. Source sample
clocks and byte ranges stay literal while the compatibility packet clocks
reflect the edit.
For regular front-metadata input with one finite media extent,
`compat::mov::forward_encoded_edits` accepts checked encoded edit plans
and delivers projected packets in source byte order. AAC packets include the
decoder priming and end-of-edit trim needed for PCM playback. The index refuses
edits that require revisiting media bytes.
`Mp4RangePlayer::prepare_seek_with_opus_recovery` includes the regular
Opus track's declared preroll. Decode from the reported first sample and
present PCM from the video landing through the track's edited end. A refused
recovery leaves the active playback generation and ordinary seek available.
For a file with surplus timing entries, `Mp4Open::with_timing_policy` can
use the timing prefix covered by the sample sizes for forward playback and
`Mp4RangeOpen`. Each track's table reports the unused timing occurrences. The source playback view
requires exact coverage. `subp` tracks are subtitle tracks.

The FFmpeg-compatible packet view opens with `Mp4Open::new_recovery`. When
the opener read the metadata before any media
(`MetadataReadScope::FrontMetadataBeforeMdat`, which a nonseekable source with
a front `moov` gives), it indexes with `Mp4IndexJob::new_native_terminal_v1`
and reads with `Engine::mp4_native_terminal_v1`; otherwise it indexes with
`Mp4View::PinnedMov9NoParseNoFillin` and reads with `Engine::mp4`. It recovers
what FFmpeg recovers: a first sample outside the source ends the view with
`MissingSample`, a truncated sample becomes a corrupt
`ShortPacket` carrying the available prefix, a zero media timescale
reads in a 1/1 base, and a zero-size read ends native delivery, with a
`EmptyPacket` where FFmpeg emits one. On a seekable source the recovery
opener refuses a zero-size fragment sample in the header, as FFmpeg does.
`Mp4Open::new` refuses out-of-source, truncated and zero-timescale media and
reads zero-size samples as declared. When a single track's first chunk lies
beyond the media read before indexing and a later chunk lies before it, a
source of unknown length may still hold that chunk in a later `mdat`: the
view reads on and refuses it as outside the media extent (`Invalid`) at the
physical end, or as an unsupported layout once the source passes its start.

**MPEG-TS.** `ts_native_program_v1` selects up to 64 PIDs of one program.
Private stream type `0x06` is audio only after explicit AC-3/E-AC-3 or DVB
admission, and a subtitle track when a DVB namespace names DVB subtitling or
teletext; stream type `0x90` (PGS) is a subtitle track. M2TS prefix startup (`compat::pes::startup::Startup::new_m2ts_prefix`) needs an
8 KiB input window. The DVB native PES parser caps a continued segment at
65,536 bytes. PES outputs reuse their payload storage's budget handle, which
remains valid while a caller retains an output after closing the player.
Delivery without a seek uses the presence of the projected presentation
timestamp directly.

**Sinks.** `pump::Sink` owns accepted `Feed` envelopes until their callbacks
retire; `Submission::Blocked` returns the same envelope. Threading and
platform decoder configuration are the sink's responsibility; no `Send` bound
is imposed.

## Conformance

- MP4: ISO/IEC 14496-12 and the [QuickTime seek tables](https://developer.apple.com/documentation/quicktime-file-format/seeking_with_a_quicktime_file).
- MPEG-TS: ITU-T H.222.0.
- AVC/HEVC parameter sets: ITU-T H.264 §7.3 and Annex B, ITU-T H.265.

Integration tests in `tests/` replay authored and FFmpeg-generated fixtures
and compare packet order, bytes, flags, PTS, DTS and duration with pinned
FFprobe records. `tests/native_terminal_matrix.rs` reads 39 terminal-media
inputs through the native packet view in 13 source geometries: 12
nonseekable (known and unknown length, read sizes 1, 7 and 4096, work quanta
1 and 4096) for every input, and one seekable read of known length for each
of the 31 original inputs, 499 rows in all, run in debug and release. It compares every packet object
with FFprobe 9.0.2, and where FFprobe refused the header it requires the run
to refuse while opening, before any packet. With `Mp4Open::new_recovery`
499/499 rows pass: 497 identical packet lists and 2 matching header refusals.
With `Mp4Open::new` 446/499 pass; the test pins the other 53 outcomes.

The synthetic `forward-aac-priming.mp4` fixture uses a 64 × 48 AVC test
pattern at 12 fps and a 440 Hz mono AAC tone at 44.1 kHz. To reproduce it with
FFmpeg 9.0.2 and libx264, run:

```sh
ffmpeg -nostdin -v error \
  -f lavfi -i 'testsrc2=s=64x48:r=12' -f lavfi -i 'sine=f=440:r=44100' \
  -t 0.35 -map 0:v:0 -map 1:a:0 -c:v libx264 -threads:v 1 -profile:v baseline \
  -pix_fmt yuv420p -g 2 -bf 0 -c:a aac -b:a 40k -movflags +faststart forward-aac-priming.mp4
```

`forward-aac-tail-trim.mp4` keeps the 44,100 Hz movie timescale and sets
the audio edit to media time zero and duration 16,459.
`forward-aac-fractional-edit.mp4` sets the movie timescale to 1,000, its
duration and the video edit's duration to 334, and the audio edit's media
time and duration to 64 and 45. The audio edit rounds to an end one sample
into the third packet. All three fixtures use the repository's dual license.
The four fragmented fixtures also use the repository's dual license.
`forward-fragment-aac-edit.mp4` and `forward-fragment-aac-late-track.mp4`
use an FFmpeg-generated AVC test pattern and mono AAC tone. Both carry an
open-ended audio initialization edit. The latter starts with a video-only
fragment. `forward-fragment-aac-short.mp4` truncates an authored AVC/AAC
fragment inside its first AAC packet. `forward-fragment-mp3-edit.mp4` uses a
generated mono MP3 tone with an initialization edit to exercise the same clock
projection for a different MPEG-4 audio object type. The
[`mp4-forward` corpus](../../harness/features/mp4-forward/corpus.json) records
their hashes and compares source byte offsets, reduced time bases and AAC trim
along with the packet data.

Fixture provenance: [AV1](tests/fixtures/av1/README.md),
[Matroska feeds](tests/fixtures/mkv-feed/README.md),
[Dolby clocks](tests/fixtures/dolby-clock/README.md),
[subtitle probes](tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](tests/fixtures/subtitles/README.md),
[terminal-media FFprobe records](tests/fixtures/terminal-matrix/README.md).
The `triergon-hls`, `triergon-bridge` and `triergon-raw-video` tests read their fixtures here too.
Gates and measurement policy: [docs/quality.md](../../docs/quality.md).
