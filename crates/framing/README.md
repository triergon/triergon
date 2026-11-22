# triergon-framing

Packet framing and codec-configuration parsing for the format layer: H.264/HEVC
NAL units and records, AV1 OBUs, VP9 records, AAC, AC-3/E-AC-3 and DTS core, and
the headers and canonical configurations of FLAC, Opus, Vorbis and MPEG audio.
Containers hand it record bytes and complete samples; it returns source-preserving
views, decoder-ready Annex-B/ADTS/temporal-unit output and typed refusals. It
depends on `triergon-core` and `triergon-bitstream`, does no I/O and does not decode
audio or video.

Each format is a module: `nal`, `avc`, `hevc`, `av1`, `vp9`, `aac`, `ac3`, `dts`,
`flac`, `opus`, `vorbis`, `mpeg_audio`, and `picture_output` for the decoded
picture buffer. The crate root holds only the step results they share,
`BuildStep` and `ConfigProgress`.

## Usage

Every operation takes a core `Budget` pool, a `WorkBudget` and a cancel flag, and
is driven by `step`. Record and packet jobs return `BuildStep::{Yield, Ready,
Complete}`; incremental readers return core `Step::{NeedInput, Yield, Ready, End}`
and are fed with `provide` and closed with `finish` or `fail`.

AVC and HEVC record parsers, including their compatibility wrappers, also offer
`step_into` with an empty output slot and `ConfigProgress`. This publishes the
configuration directly into caller storage. Occupied slots are rejected without
advancing the parser; `step` retains the same progress and error behaviour.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_framing::BuildStep;
use triergon_framing::avc::{
    AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits, AvcConfigLimits, AvcConfigParser,
    AvcConfigSource,
};

let pool = Budget::new(1 << 20)?;
let cancel = AtomicBool::new(false);
let mut work = WorkBudget::new(4096);

// avcC box payload (4-byte lengths, one SPS, one PPS) -> checked record view.
let avcc = [
    1, 0x42, 0x00, 0x1e, 0xff, // version, profile, compatibility, level, lengthSizeMinusOne 3
    0xe1, 0, 4, 0x67, 0x42, 0x00, 0x1e, // one SPS
    1, 0, 4, 0x68, 0xce, 0x3c, 0x80, // one PPS
];
let source = AvcConfigSource { source: SourceId(1), generation: 0, offset: 0 };
let limits = AvcConfigLimits { maximum_record_bytes: 4096, maximum_nals: 64, maximum_nal_bytes: 4096 };
let mut parser = AvcConfigParser::new(&pool, source, Bytes::copy_from_slice(&pool, &avcc)?, limits)?;
let record = loop {
    match parser.step(&mut work, &cancel)? {
        BuildStep::Ready(record) => break record,
        BuildStep::Yield => work = WorkBudget::new(4096),
        BuildStep::Complete => return Err("no record".into()),
    }
};
let config = AvcAnnexBConfig::new(record)?;

// One length-prefixed MP4/Matroska sample (an IDR slice) -> Annex-B segments.
let sample = [0, 0, 0, 3, 0x65, 0x88, 0x84];
let limits = AvcAnnexBLimits {
    maximum_packet_bytes: 1 << 20,
    maximum_output_bytes: 1 << 20,
    maximum_nals: 256,
};
let mut job = AvcAnnexBJob::new(&config, &sample, limits)?;
let mut decoder_input = Vec::new();
loop {
    match job.step(&mut work, &cancel)? {
        BuildStep::Ready(unit) => unit.segments().for_each(|s| decoder_input.extend_from_slice(s)),
        BuildStep::Yield => work = WorkBudget::new(4096),
        BuildStep::Complete => break,
    }
}
// The record's SPS and PPS precede the IDR slice, each after a start code.
assert_eq!(decoder_input, [
    0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e,
    0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80,
    0, 0, 0, 1, 0x65, 0x88, 0x84,
]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Supported

| Module | Entry points | What it does |
|---|---|---|
| `nal`: streams | `NalReader`, `Framing::{AnnexB, LengthPrefixed}`, `LengthSize` | Incremental AVC/HEVC NAL extraction from any chunking; 1–4 byte big-endian lengths or Annex-B start codes; fixed NAL header facts |
| `nal`: units in memory | `AnnexBUnits`, `start_code`, `split_length_prefixed`; `NalHeader::parse` | An access unit's or stream's NAL units without their start codes or length prefixes; validated fixed headers |
| `nal`: parameter-set walks | `skip_vui_to_timing`, `skip_hevc_profile_tier_level` | Pass over the VUI fields before the timing, or H.265 profile-tier-level, over any `BitSource`, so each caller keeps its own emulation-prevention strictness and errors |
| `nal`: raw Annex-B access units | `RawAnnexBReader`, `RawAccessUnit` | Groups an elementary AVC/HEVC stream into access units (H.264 §7.4.1.2.3–4, H.265 §7.4.2.4.4; an HEVC end of sequence or bitstream starts the next one, as in FFmpeg's parser); reports IDR/IRAP; keeps original start codes and padding |
| `nal`: FFmpeg-compatible BSF | `NalPacketSession`, `NalPacketConfigWriter`, `compat::nal` | Stateful `h264_mp4toannexb`/`hevc_mp4toannexb` packet behaviour, including in-band parameter-set tracking and AVC `NEW_EXTRADATA` |
| `nal`: other config inputs | `AnnexBConfigParser`, `DeclaredConfigParser`, `Configuration::declared_record`; `AnnexBWriter` | Annex-B extradata framing; caller-declared array/NAL layouts; opaque record retention; a single NAL with `00 00 00 01` |
| `avc`, `hevc`: type values | `avc::{SLICE, IDR, SEI, SPS, PPS, AUD}`; `hevc::{VPS, SPS, PPS, …}`, `hevc::{is_slice, begins_access_unit, is_irap, is_idr, is_bla, is_rasl, is_radl}` | H.264 and H.265 `nal_unit_type` values and the HEVC type classes (slice, access-unit start, IRAP, IDR, BLA, RASL, RADL) |
| `avc`: headers | `read_sps`, `read_pps`, `read_slice_header`, `slice_pps_id`; `AvcSps`, `AvcVui`, `AvcPocSyntax`, `AvcPps`, `AvcSliceHeader`, `AvcSliceDepth`, `AvcReading` | The SPS, PPS and slice header fields that picture order and output need (order count syntax, `frame_num` and `gaps_in_frame_num_value_allowed_flag`, fields, VUI timing and bitstream restriction, `no_output_of_prior_pics_flag`, `memory_management_control_operation` 5); `00 00 00` to `00 00 02` inside an RBSP refused or read as data |
| `avc`, `hevc`: parameter-set prefixes | `avc::{read_sps_raster, read_sps_timing, read_sps_reorder, read_pps_ids, read_slice_prefix}`, `AvcRaster`, `AvcVuiTiming`; `hevc::{read_vps_id, read_vps_timing, read_sps_prefix, read_sps_timing, read_sps_reorder, read_pps_ids, read_slice_prefix, check_parameter_sets}`, `HevcSpsPrefix`, `HevcVuiTiming` | What a container reader needs from a parameter set or slice header without decoding: identifiers, the display raster after cropping, VPS and VUI timing as coded, and the reorder limit |
| `avc`: avcC | `AvcConfigParser` | Full record: fixed fields, SPS/PPS/SPS-ext arrays, high-profile extension, trailing bytes |
| `hevc`: hvcC | `HvcConfigParser`, `inspect_config_prefix` | Version 1: 23-byte fixed prefix, all NAL arrays in order (unknown/duplicate/empty arrays kept) |
| `avc`, `hevc`: sample to Annex-B | `AvcAnnexBJob`, `HevcAnnexBJob` | Four-byte start codes; prepends record SPS/PPS (VPS/SPS/PPS) before IDR/IRAP; zero-copy segments |
| `hevc`: record to Annex-B | `HvcAnnexBWriter`, `HvcDecoderConfigWriter` | All hvcC NALs in order, or the decoder subset (types 32–34, 39, 40) |
| `picture_output` | `{DecodedPictures, Entry, View, NewPicture, Leave, Marking, Parity, PictureId, HevcLimits, HevcPicture, AvcPicture, StoreError, Refused, LateOutput}`, `order::{AvcOrder, HevcOrder, AfterMmco5, AfterGap}`, `levels::{max_dpb_mbs, max_dpb_frames}` | The decoded picture buffer of H.264 Annex C.4 (gap frames C.4.2, C.4.4, C.4.5, field pairs, bumping C.4.5.3) and H.265 Annex C.5.2 (C.5.2.2–C.5.2.4), generic over the stored picture, driven by marking, removal, storage and flush events, each stored picture readable by position and markable by field, and plain storage and bumping for a caller with its own output conditions; the output of the H.264 and HEVC decoders and of caption extraction; picture order counts of H.264 8.2.1 (types 0, 1, 2, `memory_management_control_operation` 5, `frame_num` gaps) and H.265 8.3.1; MaxDpbMbs and MaxDpbFrames of Table A-1; FFmpeg's observed departures as separate steps and options |
| `av1` | `Av1ObuReader`, `Av1ConfigParser`, `Av1TemporalUnitJob` | Low-overhead or single-OBU framing with exact LEB128; av1C v1 fields and config OBUs; container sample to decoder temporal unit |
| `vp9` | `Vp9ConfigParser` | vpcC v1 fields (profile, level, bit depth, chroma, range, CICP) |
| `aac`: configuration | `AacAscParser`, `AacAscScopeParser`, `decoder_configuration_view` | AudioSpecificConfig for AAC-LC (2), SBR (5), PS (29): frequency, fixed channel configurations 1–7, 11, 12 and 14, GASpecificConfig, PCE, `0x2b7`/`0x548` sync extensions |
| `aac`: ADTS | `inspect_adts_header`, `AacAdtsParser`, `AacAdtsWriter`, `AacAdtsStreamReader`, `AacAdtsCarrierReader` | 56-bit header facts; single-block AAC-LC frames; ADTS wrapping of raw access units (`AacAdtsParameters::from_asc`); aligned ADTS streams |
| `ac3` | `probe_header`, `Ac3FrameReader`, `Ac3ConfigParser`, `Eac3BsiParser` | Header facts (bsid 0–8, 11–16), aligned syncframe reader, dac3/dec3 records, complete E-AC-3 BSI |
| `dts`: core | `probe_core_header`, `DtsCoreFrameReader`, `DtsHeaderInspector` | Normal and termination core headers (all fields, optional HCRC), FSIZE+1 frame acquisition |
| `dts`: declared extents | `probe_frame_extent`, `probe_extension_substream_extent` | Bounded core and extension size fields at an established canonical sync, including the short and long extension prefixes |
| `dts`: packings | `Dts14BitUnpacker`, `DtsPackedPayloadJob`, `Dts14SyncReader`, `Dts14CarrierReader`, `DtsNormalSourceReader` | 16-bit LE and 14-bit BE/LE word packings; experimental 14-bit stream acquisition |
| `reference_trace::dts` | `ParserOperation` (`reference-trace` feature) | Raw 14-bit source frames alongside the packets, clock and byte positions of FFmpeg's `dca` parser |
| `flac` | `stream_info`, `metadata_blocks`, `FlacStreamInfo`, `FlacFrameHeader`, `canonical_configuration` | STREAMINFO and the metadata blocks of a stream header, frame headers with their CRC-8 (RFC 9639); the canonical configuration (`fLaC` and metadata blocks) of a native header, an ISO BMFF `dfLa` payload or the first Ogg FLAC packet |
| `opus` | `OpusHead`, `packet_samples`, `SAMPLE_RATE`, `canonical_configuration` | The identification header of RFC 7845 and its `dOps` form; packet durations from the TOC (RFC 6716); the canonical configuration (an `OpusHead` packet) |
| `vorbis` | `VorbisHeaders`, `VorbisIdentification`, `VorbisModes`, `header_type`, `canonical_configuration` | The three header packets and their Xiph lacing; mode block sizes from the setup header, so a container derives packet durations without decoding |
| `mpeg_audio` | `MpegAudioHeader`, `info_frame`, `MpegInfoFrame`, `LameTag` | The 32-bit frame header of ISO/IEC 11172-3 and 13818-3 with the MPEG-2.5 rates; Xing, Info and VBRI frames with the LAME tag |

The framing views harness compares aligned DTS core frames and declared extents
with FFmpeg's DTS parser and `dca_core` filter. Its header oracle checks every
field, bit range and derived view from both inspectors. A non-core sync leaves
all later core fields unread. The packed-reader
comparison covers 14-bit big- and little-endian words and 16-bit little-endian
words. A 16-bit source ending immediately after the core sync reports a
truncated header. The unpacking oracle checks canonical bytes and physical
bit locations,
with reference-decoded PCM equality on real first-frame repackings. The parser
trace comparison checks packets, duration, rate, profile and byte hints at
multiple input window sizes. The comparison reports the core clock with
the profile selected by XCh, XXCh or X96, and retains the core profile when
only an incomplete extension-substream marker follows the core.

Prepare the shared inputs with `cargo xtask framing-views fixtures dts`. The
[feature results](../../docs/status.md#dts-core-header-inspection) link the five
contracts and record their open cases.

With the DTS packet corpus in `TRIERGON_DTS_CORPUS` and the pinned `ffmpeg`
and `ffprobe` on `PATH`, run
`cargo test -p triergon-framing --test dts_packet_corpus -- --ignored`.
The comparison uses the second audio stream of `dts.mp4`, `dts.mkv` and
`dts.mov`, plus the FATE DTS-HD MA sample and its three remuxes in `fate-dts/`.
It compares the 27 fields between inspectors. For every input packet, the
reference filter supplies the expected core size and bytes. The [DTS measurement results](../../docs/status.md#dts-core-header-inspection)
cover the seven inputs and the native instruction counts for header inspection.

## Behaviour and limits

`AacAdtsCarrier::into_selected_frame` shares the original bytes of a complete,
aligned AAC-LC envelope with the typed frame. On an opaque or incomplete
carrier it returns the original observation, including its boundary and
selected-source diagnostic.

**Reference compatibility.** The types of the format modules follow the specifications.
`compat::nal` reproduces FFmpeg where a consumer needs FFmpeg's result: it accepts
configuration records as the `mp4toannexb` packet receiver does, and names the
FFmpeg revision it reproduces. Comparison-only projections are compiled only with
the non-default `reference-trace` feature: the reference's return codes, and under
`reference_trace::dts` the packets, clock and byte positions of FFmpeg's `dca`
parser over raw 14-bit DTS.

**Input model.** Incremental readers own a core `InputCursor` and accept same-pool
windows of any size with the exact token, source and offset. An empty window is
starvation; a known length is not EOF. Truncation, transport failure and
cancellation are sticky errors, never a synthetic end. A new source needs a new
reader, and so does a restart, except in the ADTS readers and the DTS packing
readers, which restart on their source with `reset`. Record parsers take a complete
pool-owned record (box payload, no size/type header); sample jobs take one complete
container sample.

**Source preservation.** Outputs keep exact source bytes, offsets, source ID and
generation. Raw and reserved fields stay readable; local violations are reported
through `syntax_issues()` bitsets rather than cleared. Absent and present-zero
fields are distinct (`None` vs `Some(0)`). Views share the pool-owned input. A
retained view keeps the whole backing allocation charged. The configuration
writers use the validated parameter-set ranges from these records.

**Resources.** All arithmetic is checked. Every operation has caller limits
(bytes, NAL/OBU/frame counts, input window). Work is charged per byte or field;
one unit always progresses and zero units consume nothing. Copies are at most
4096 bytes between cancel checks. Allocation pressure returns retryable
`Backpressure` at the exact checkpoint; no partial output is published.
Stateful packet jobs keep up to seven segment descriptors inline, enough for an
access unit with in-band parameter sets and several slices, and charge heap
segment storage for more to the caller's pool; they share the original payload
for passthrough packets. A packet or configuration
output is sized before it is written and charged once, with the shared owner it
is published in; one of at most 4096 bytes that the remaining work covers is
copied in one pass, charged as its NAL-by-NAL copy is. A `NalPacketSession`
frames packets, configuration records and outputs of at most 4 GiB − 1 bytes
(`NalPacketLimits::MAXIMUM_SPAN`), whatever its limits allow; its segment
descriptors are 16 bytes.

**Codec-specific limits.**

- NAL: payload bytes are opaque. No RBSP, SPS/PPS body or decoder-consistency
  check. Annex-B termination follows H.264 B.2 / H.265 B.3; the leading `zero_byte`
  rule is not enforced. `AnnexBWriter` never edits emulation prevention.
- avcC/hvcC parsers refuse `lengthSizeMinusOne = 2` (three-byte lengths) and
  unknown versions. The `compat::nal` receiver parsers accept them for the packet
  session only.
- The AVC packet receiver can emit parameter sets before a buffering-period SEI
  (H.264 Annex D). It uses in-band sets found before the following slice, then
  falls back to the active configuration for missing sets. It preserves the
  original packet NALs and does not parse the SEI payload.
- The AVC High-profile comparisons frame samples from complete sequence and
  picture parameter-set arrays with zero to three extension header bytes.
  The receiver preserves those bytes as a trailing suffix and reports
  `Truncated` as the strict source refusal. When a complete extension header
  declares entries and the record ends at the header, the receiver frames
  the base parameter sets and preserves the header and the strict refusal.
- `RawAnnexBReader` refuses data-partitioned, redundant-picture and extended AVC
  profiles. It adds no timestamps. `append_payload` can consume an interior NAL
  slice while the reader is waiting for input. A fresh reader can use `resume_at`
  at an independently established access-unit boundary, preserving the original
  elementary coordinate and preceding NAL count. Resumed bytes must carry their
  own parameter sets; no decoder or picture history is restored.
- AV1: LEB128 up to 8 bytes and `u32::MAX`. `OneObu` needs an external length;
  AV1 Annex-B (length-delimited) streams are not parsed. Sequence headers are not
  parsed. `Av1TemporalUnitJob` adds a missing temporal delimiter, inserts av1C
  config OBUs at random access when the sample has no sequence header, and writes
  a size field for a size-less final OBU.
- VP9: only vpcC version 1. No frame header or superframe parsing.
- AAC: other object types, unsupported extension modes and frequency indices
  13/14 are refused. ADTS wrapping refuses configurations above 7 because the
  header carries three channel-configuration bits. Single-block ADTS frames retain the CRC word when present.
  The parser does not verify the CRC and refuses multi-block frames. The writer obeys the 8191-byte frame limit.
- AC-3/E-AC-3: input must be aligned big-endian syncframes. There is no resync
  search, no word-swapped or IEC 61937 input, and no CRC or audio-block check.
  bsid 9/10 and E-AC-3 mixing definition 3 are refused. `sample_span()` is the
  coded duration of one frame, not a presentation time.
  `Ac3FrameReader::read_frame_parts` can consume a complete aligned frame
  from several slices when the first slice contains its header prefix.
- DTS: `DtsCoreFrameReader` reads canonical big-endian normal and termination
  frames without resynchronizing. A termination frame reports its coded
  window and its padding sample count separately. The extension extent probes read size fields. HCRC is reported, not
  checked. `DtsHeaderInspector` and `inspect_header_bits` expose the raw
  seven-bit NBLKS code once complete. Their `coded_window_samples` view is
  `32 × (NBLKS + 1)` core samples per channel for codes 5 through 127.
  `normal_window` additionally requires a normal frame, SHORT=31, a normal
  block count (7, 15, 31, 63 or 127), a known sample rate and version 0–7.

## Conformance

Implemented from these specifications:

| Area | Reference |
|---|---|
| H.264 NAL / Annex B / AU | ITU-T H.264 (08/2024) §7.3.1, §7.4.1 Table 7-1, §§7.3.2.1.1, 7.3.2.2, 7.3.3, 7.4.1.2.3–4, Annex B |
| HEVC NAL / Annex B / AU | ITU-T H.265 (07/2024) §7.3.1.2, §7.4.2 Table 7-1, §7.4.2.4.4, Annex B |
| Picture output | ITU-T H.264 (08/2024) §8.2.1, §8.2.5.3–4, §A.3.1 Table A-1, Annex C.4.4–C.4.5; ITU-T H.265 (07/2024) §8.1.3, §8.3.1–8.3.2, Annex C.5.2 |
| avcC | ISO/IEC 14496-15:2010 with the 2006 FRExt amendment |
| hvcC, AAC ASC/ADTS | ISO/IEC 14496-15 and 14496-3 record layouts, from licensed interoperability references |
| AV1 | AV1 Bitstream v1.0.0 Errata 1 §§4.10.5, 5.2–5.3, 6.2; AV1-ISOBMFF v1.3.0 §§2.3.3–2.4 |
| VP9 | VP Codec ISO Media File Format Binding v1.0 |
| AC-3 / E-AC-3 | ATSC A/52:2018 §§5.3–5.4, Table 5.18, Annex E Tables E1.2, E1.5, E1.6 and §E2.3; ETSI TS 102 366 V1.4.1 Annex F.4/F.6 (dac3/dec3) |
| DTS | ETSI TS 102 114 V1.6.1 §§3.4, 5.3, 5.4.2, Tables 5-1 to 5-20; V1.2.1 §5.3 (14-bit packing) |

Verification:

- `fixtures/*.tsv` are bit-authored corpora with expected values from an
  independent whole-buffer oracle. Tests replay each case at many chunk sizes and
  work grants, plus negative, limit, cancellation, backpressure and ownership cases.
- `tests/nal_packet_receiver.rs` and `tests/framing_native_compat.rs` encode
  behaviour observed from pinned FFmpeg (`h264_mp4toannexb`, `hevc_mp4toannexb`,
  hvcC and AAC configuration handling).
- `fixtures/raw-annex-b/*.packets` hold the packet extents and key flags FFmpeg's
  raw H.264/HEVC demuxers produce for the paired streams. `hevc-eos.hevc` is
  `hevc-aud0.hevc` with an end of sequence before its second VPS and an end of
  bitstream at the end.

From the repository root, `cargo xtask framing` compares all 135 AVC/HEVC
packet-framing scripts with the pinned FFmpeg bitstream-filter API. The
[contract](../../harness/features/packet-framing/contract.json) covers configuration,
complete packet fields and side data, malformed input, flush and drain actions,
and retained packet access after close. Add `--benchmark --memory` to collect
paired runtime and memory measurements through the shared harness; qualification
requires the pinned gate build on native AArch64 Linux.

The framing views harness compares incremental AVC and HEVC length-prefixed
units, Annex B configuration and caller-declared record layouts, and both
hvcC-to-Annex-B writers. The reader comparison covers one- through four-byte
length fields at three window sizes. It compares each unit returned by the
reference converter with the corresponding length-delimited source span.
The configuration comparison checks
ordered unit types and bytes, retained opaque source bytes, and four-byte
start codes from `AnnexBWriter`. The HEVC writer comparison also checks the
decoder writer's refusal type.

Prepare the inputs with `cargo xtask framing-views fixtures nal`. The
[NAL comparison results](../../docs/status.md#linux-aarch64) link the three
contracts and describe the open differences and measurement coverage.

The framing views harness also compares AV1 OBUs returned by FFmpeg's
`filter_units` and ADTS frames returned by the raw AAC demuxer. The ADTS
comparison uses `aac_adtstoasc` for configured frames and the demuxed payload
for in-band program configurations and empty frame bodies. The AV1 comparison
covers low-overhead and externally sized OBUs, including complete OBUs before
a truncated size field. Independent wire
oracles check AV1 offsets and exact LEB128 spellings, and all ADTS header
fields, optional CRC words and original payloads. The AV1 wire oracle uses
three input window sizes for every case.

Prepare these inputs with `cargo xtask framing-views fixtures streams`.
The [stream parser results](../../docs/status.md#linux-aarch64) link both
reference comparisons and their wire oracles.

The configuration comparisons check fixed hvcC and vpcC fields and complete
E-AC-3 bitstream information against wire oracles. The pinned FFmpeg workers
check the range and CICP values a VP9 movie publishes, the HEVC profile, level
and pixel format from the first packet of a muxed stream, AAC channel decisions
at both ASC extents, and E-AC-3 packet geometry, layout and decoder side data.
The AAC cases also check the bytes MOV supplies from a decoder descriptor and
the rate, channels and sample count from a first access unit. A two-byte LC
configuration describes the core clock, so the comparison expresses a decoded
dual-rate SBR frame in core samples at the signalled rate.

Prepare the inputs with `cargo xtask framing-views fixtures config`. The
[configuration comparisons](../../docs/status.md#linux-aarch64) list the
parity results and open cases.

Set `TRIERGON_PACKET_EVENTS` to a private output directory when running
`cargo test -p triergon-framing --features reference-trace --test packet_view`
to export every complete event trace and digest with all digest assertions active.

Per-module parity, runtime and memory results are in
[docs/status.md](../../docs/status.md); the gates and harness are described in
[docs/quality.md](../../docs/quality.md).
