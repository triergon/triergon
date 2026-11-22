# triergon-bridge

Frames [`triergon-playback`](../playback/README.md) packets for decoders and
feeds text, bitmap, caption and teletext packets to subtitle receivers.

| Area | Entry points |
|---|---|
| Matroska feeds | `matroska::MatroskaFeedSetup` (AVC Annex B, AAC ADTS), `MatroskaHevcDolbySetup` (HEVC Annex B, AC-3/E-AC-3), `MatroskaDiscovery`, `MatroskaCompactDiscovery`, `MatroskaVideoSeekGate` |
| Matroska clocks | feed methods `demux_avc_clock`, `demux_aac_clock_probe` (`MatroskaTrackFeed`), `demux_hevc_clock`, `demux_dolby_clock_probe` (`MatroskaHevcDolbyFeed`); the audio probes are `MatroskaClockProbe`s; `MatroskaBitmapClockProbe` |
| Access units | `access::AccessUnits` from a container track's `TrackInfo` and its packets' `PacketInfo`: AVC and HEVC as Annex B, AAC with an AudioSpecificConfig as ADTS, other codecs and tracks without a record as they are; `AccessUnits::with_record` where the record comes from elsewhere |
| Packet bridges | `aac::AacBridge` (MP4 AAC as ADTS, bound to one plan's track and configuration revision), `av1::Av1Bridge` (including MP4 `av01` and `dav1`) |
| Subtitles | `text::TextBridge`, `bitmap::{PgsBridge, DvdReceiver}`, `matroska::MatroskaBitmapTrack`, `mp4_dvd::{Mp4DvdSetup, Mp4DvdTrack}`, `ts::dvb::DvbTsBridge`, `ts::pgs::PgsTsTrack`, `ts::teletext::TeletextTsBridge`, `caption::CaptionBridge` |
| Compatibility | `compat::matroska::legacy_aac_lc_initialization` |

## Behaviour and limits

**Compact Matroska replay.** Inspect the replay prefix before delivering its
events in order. `MatroskaReplayPrefix::feed_packet` checks track selection and
configuration for buffered AAC or AVC.

**Matroska HEVC discovery.** Startup requires one distinct parameter set of
each type. It accepts byte-identical repeats in CodecPrivate and before the
first slice of an access unit. Configuration array types must match parameter
set NAL headers. Repeated VPS timing requires positive tick units and time
scale when those fields are present. The single-layer VPS is checked through
its trailing bits, the SPS through its timing fields, and the base PPS through
its extension flags. The demux packet clock also accepts matching in-band
sets before a picture's first slice.

**Access units.** Feed `Output::packet` from Matroska or MP4 into `AccessUnits`.
The access-unit readers in `triergon-hls`, `triergon-raw-video` and
`triergon-mpegts` already frame Annex B and ADTS. For a decoder that takes
contiguous input, join a sliced unit through `contiguous_feed` or `segments`.
`Event::DemuxPes` and PES carried in `Event::Ts` have no packet view.
`AccessUnits::for_track` refuses Matroska AAC without CodecPrivate. For legacy
AAC-LC, pass the derived AudioSpecificConfig to `AccessUnits::with_record`.

**Legacy Matroska AAC.** Discovery and `MatroskaFeedSetup` accept
`A_AAC/MPEG2/LC` and `A_AAC/MPEG4/LC`. When a track has no initialization
record, `compat::matroska::legacy_aac_lc_initialization` derives AAC-LC
configuration from the Audio element's indexed sampling frequency and a count
of one to six channels. A present Audio element can use the EBML defaults for
its children. Larger layouts require explicit initialization. The feed
preserves the source packet bytes.
The legacy compatibility clock keeps a missing duration unknown.

**Generations.** Feeds, bridges, clocks and subtitle receivers are bound to one
playback generation and configuration; rebuild them after a seek, track switch
or configuration change.

**Matroska clocks.** The Dolby clock probe retains at most 4,096 timing rows and
inspects up to 32 syncframes to infer a missing duration.
Demux clock errors identify a wrong packet codec, a mismatched feed, or a
video sample that cannot extend the decode clock.

**Subtitles.** The TS bridges bind to one PID of one active PMT and take its
strict private-stream PES; a new generation, PMT epoch or clock epoch needs a
new bridge. Their times are the PES times on the player's timeline; teletext
times are bounded by the program clock as FFmpeg's demuxer bounds them, read
from the transport packets offered to `TeletextTsBridge::observe`.
`PgsTsTrack` hands each PES to `PgsBridge`. `Mp4DvdSetup` reads the colour
table of an `mp4s` entry, and its `Mp4DvdTrack` hands each sample to
`DvdReceiver`; a subpicture without a stop ends with its packet's duration.
A Matroska VobSub track's `vobsub_settings` reads its palette and canvas; the
DVD queue places pictures on that canvas.
Teletext pages and closed captions become opaque ASS `TextSource`s, which
`TextBridge` reports as it reports a Matroska ASS track's cues; an empty
teletext page is a clear. A teletext decode needs the decoder's whole packet
work in one budget. `CaptionBridge` takes H.264 or HEVC access units or MPEG-2
PES payloads in decode order and decodes the caption frames FFmpeg's
extraction settles, in its order. It is made with the track's `avcC` or `hvcC`
record when its samples are length-prefixed, again after a seek: samples often
carry no parameter sets, and pictures without them lose their captions. It
charges one unit per packet byte and,
per frame, the receiver's packet work for its size, each paid over as many
polls as needed.

## Conformance

- Matroska: [codec mappings](https://www.ietf.org/archive/id/draft-ietf-cellar-codec-21.html).
- AV1 per the [AV1-ISOBMFF sample format](https://aomediacodec.github.io/av1-isobmff/v1.3.0.html#av1-sample-format).
- DVB subtitles per
  [ETSI EN 300 468 §6.2.41](https://www.etsi.org/deliver/etsi_en/300400_300499/300468/01.18.01_60/en_300468v011801p.pdf),
  teletext per ETSI EN 300 472.
- Closed captions: ATSC A/53 Part 4 `cc_data()` in ITU-T T.35 SEI messages.

The [access-unit comparison](../../harness/features/bridge-access-units/contract.json)
checks stream identity, source clocks, key and configuration flags, and payload
bytes. The [AAC packet bridge](../../harness/features/bridge-mp4-packets/contract.json)
compares complete ADTS frames with the FFmpeg muxer. The [AV1 packet
bridge](../../harness/features/bridge-av1-low-overhead/contract.json) compares
temporal-unit bytes with an independent low-overhead bitstream oracle for MP4
and WebM. The [Matroska discovery comparison](../../harness/features/bridge-matroska-discovery/contract.json)
checks discovered codec fields, PGS canvas, legacy AAC initialization, source
replay and the HEVC seek landing. The AAC audio-clock comparison exercises
the compact replay feed through its clock and raw packet output.

The [text subtitle comparison](../../harness/features/bridge-text-subtitles/contract.json),
[bitmap subtitle comparison](../../harness/features/bridge-bitmap-subtitles/contract.json)
and [caption and teletext comparison](../../harness/features/bridge-captions-teletext/contract.json)
feed real container packets from `Player` through the bridges. They compare
cue bytes, bitmap geometry and pixels, palettes, and presentation clocks
against the corresponding FFmpeg decoder outputs.

The integration tests (`tests/it`) read the fixtures of `crates/playback/tests/fixtures`.
Fixture provenance: [AV1](../playback/tests/fixtures/av1/README.md),
[Matroska feeds](../playback/tests/fixtures/mkv-feed/README.md),
[Dolby clocks](../playback/tests/fixtures/dolby-clock/README.md),
[subtitle probes](../playback/tests/fixtures/matroska-subtitle-probe/README.md),
[subtitle bridges](../playback/tests/fixtures/subtitles/README.md).
