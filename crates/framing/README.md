# triergon-framing

Packet framing and codec-configuration parsing for the format layer: H.264/HEVC NAL units
and records, AV1 OBUs, VP9 records, AAC, AC-3/E-AC-3, DTS core, MPEG audio headers, and
FLAC, Opus and Vorbis configurations. Containers hand it record bytes and complete
samples; it returns source-preserving views, decoder-ready Annex-B/ADTS/temporal-unit
output and typed refusals. It depends on `triergon-core` and `triergon-bitstream`, does
no I/O and does not decode audio or video.

## Usage

Every record job, packet job and incremental reader takes a core `Budget` pool, a
`WorkBudget` and a cancel flag, and is driven by `step`. Record and packet jobs return
`BuildStep::{Yield, Ready, Complete}`; incremental readers return core `Step::{NeedInput,
Yield, Ready, End}` and are fed with `provide` and closed with `finish` or `fail`.

AVC and HEVC record parsers, including their compatibility wrappers, also offer
`step_into` with an empty output slot and `ConfigProgress`. This publishes the
configuration directly into caller storage. Occupied slots are rejected without
advancing the parser; `step` retains the same progress and error behaviour.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_framing::{
    AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits, AvcConfigLimits, AvcConfigParser,
    AvcConfigSource, BuildStep,
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

| Area | Entry points | What it does |
|---|---|---|
| NAL streams | `NalReader`, `Framing::{AnnexB, LengthPrefixed}`, `LengthSize` | Incremental AVC/HEVC NAL extraction from any chunking; 1–4 byte big-endian lengths or Annex-B start codes; fixed NAL header facts |
| NAL units in memory | `AnnexBUnits`, `start_code`, `split_length_prefixed`; `NalHeader::parse`; `avc`, `hevc` | An access unit's or stream's NAL units without their start codes or length prefixes; validated fixed headers; H.264 and H.265 type values and the HEVC type classes (slice, access-unit start, IRAP, IDR, BLA, RASL, RADL) |
| Parameter-set walks | `skip_avc_scaling_matrix`, `skip_vui_to_timing`, `skip_hevc_profile_tier_level` | Pass over an SPS's H.264 scaling matrix, VUI fields before the timing, or H.265 profile-tier-level, over any `BitSource`, so each caller keeps its own emulation-prevention strictness and errors |
| H.264 headers | `read_avc_sps`, `read_avc_pps`, `read_avc_slice_header`, `avc_slice_pps_id`; `AvcSps`, `AvcVui`, `AvcPocSyntax`, `AvcPps`, `AvcSliceHeader`, `AvcSliceDepth`, `AvcReading` | The SPS, PPS and slice header fields that picture order and output need (order count syntax, `frame_num` and `gaps_in_frame_num_value_allowed_flag`, fields, VUI timing and bitstream restriction, `no_output_of_prior_pics_flag`, `memory_management_control_operation` 5); `00 00 00` to `00 00 02` inside an RBSP refused or read as data |
| Picture output | `picture_output::{DecodedPictures, Entry, View, NewPicture, Leave, Marking, Parity, PictureId, HevcLimits, HevcPicture, AvcPicture, StoreError, Refused, LateOutput}`, `picture_output::order::{AvcOrder, HevcOrder, AfterMmco5, AfterGap}`, `picture_output::levels::{max_dpb_mbs, max_dpb_frames}` | The decoded picture buffer of H.264 Annex C.4 (gap frames C.4.2, C.4.4, C.4.5, field pairs, bumping C.4.5.3) and H.265 Annex C.5.2 (C.5.2.2–C.5.2.4), generic over the stored picture, driven by marking, removal, storage and flush events, each stored picture readable by position and markable by field, and plain storage and bumping for a caller with its own output conditions; the output of the H.264 and HEVC decoders and of caption extraction; picture order counts of H.264 8.2.1 (types 0, 1, 2, `memory_management_control_operation` 5, `frame_num` gaps) and H.265 8.3.1; MaxDpbMbs and MaxDpbFrames of Table A-1; FFmpeg's observed departures as separate steps and options |
| Raw Annex-B access units | `RawAnnexBReader`, `RawAccessUnit` | Groups an elementary AVC/HEVC stream into access units (H.264 §7.4.1.2.3–4, H.265 §7.4.2.4.4; an HEVC end of sequence or bitstream starts the next one, as in FFmpeg's parser); reports IDR/IRAP; keeps original start codes and padding |
| avcC | `AvcConfigParser` | Full record: fixed fields, SPS/PPS/SPS-ext arrays, high-profile extension, trailing bytes |
| hvcC | `HvcConfigParser`, `inspect_hvc_config_prefix` | Version 1: 23-byte fixed prefix, all NAL arrays in order (unknown/duplicate/empty arrays kept) |
| Sample to Annex-B | `AvcAnnexBJob`, `HevcAnnexBJob` | Four-byte start codes; prepends record SPS/PPS (VPS/SPS/PPS) before IDR/IRAP; zero-copy segments |
| Record to Annex-B | `HvcAnnexBWriter`, `HvcDecoderConfigWriter`, `AnnexBWriter` | All hvcC NALs in order, or the decoder subset (types 32–34, 39, 40); single NAL with `00 00 00 01` |
| FFmpeg-compatible BSF | `NalPacketSession`, `NalPacketConfigWriter`, `compat::nal` | Stateful `h264_mp4toannexb`/`hevc_mp4toannexb` packet behaviour, including in-band parameter-set tracking and AVC `NEW_EXTRADATA` |
| Other config inputs | `AnnexBConfigParser`, `DeclaredConfigParser`, `Configuration::declared_record` | Annex-B extradata framing; caller-declared array/NAL layouts; opaque record retention |
| AV1 | `Av1ObuReader`, `Av1ConfigParser`, `Av1TemporalUnitJob` | Low-overhead or single-OBU framing with exact LEB128; av1C v1 fields and config OBUs; container sample to decoder temporal unit |
| VP9 | `Vp9ConfigParser` | vpcC v1 fields (profile, level, bit depth, chroma, range, CICP) |
| AAC | `AacAscParser`, `AacAscScopeParser`, `aac_decoder_configuration_view` | AudioSpecificConfig for AAC-LC (2), SBR (5), PS (29): frequency, fixed channel configurations 1–7, 11, 12 and 14, GASpecificConfig, PCE, `0x2b7`/`0x548` sync extensions |
| ADTS | `inspect_aac_adts_header`, `AacAdtsParser`, `AacAdtsWriter`, `AacAdtsStreamReader`, `AacAdtsCarrierReader` | 56-bit header facts; single-block AAC-LC frames; ADTS wrapping of raw access units (`AacAdtsParameters::from_asc`); aligned ADTS streams |
| AC-3 / E-AC-3 | `probe_ac3_header`, `Ac3FrameReader`, `Ac3ConfigParser`, `Eac3BsiParser` | Header facts (bsid 0–8, 11–16), aligned syncframe reader, dac3/dec3 records, complete E-AC-3 BSI |
| DTS core | `probe_dts_core_header`, `DtsCoreFrameReader`, `DtsHeaderInspector` | Normal core header (all fields, optional HCRC), FSIZE+1 frame acquisition |
| DTS declared extents | `probe_dts_frame_extent`, `probe_dts_extension_substream_extent` | Bounded core and extension size fields at an established canonical sync, including the short and long extension prefixes |
| DTS packings | `Dts14BitUnpacker`, `DtsPackedPayloadJob`, `Dts14SyncReader`, `Dts14CarrierReader`, `DtsNormalSourceReader` | 16-bit LE and 14-bit BE/LE word packings; experimental 14-bit stream acquisition |
| MPEG audio | `MpegAudioHeader`, `mpeg_info_frame`, `mpeg_audio_crc_ok` | ISO/IEC 11172-3 and 13818-3 frame headers, including MPEG-2.5 rates; Xing, Info (with LAME tag) and VBRI information frames; frame CRC check |
| FLAC | `flac_stream_info`, `flac_metadata_blocks`, `flac_configuration_from_dfla`, `flac_configuration_from_ogg`, `FlacFrameHeader` | STREAMINFO and metadata blocks; the `fLaC` configuration rebuilt from a `dfLa` payload or the first Ogg FLAC packet; frame headers with their CRC-8 |
| Opus | `OpusHead`, `opus_packet_samples` | Identification header, also from a `dOps` payload; packet duration from the TOC |
| Vorbis | `VorbisIdentification`, `VorbisModes`, `vorbis_packet_blocksize`, `xiph_split`, `xiph_join` | Identification header, setup-header modes and each audio packet's block size without decoding; Xiph lacing |
| Audio configuration | `canonical_flac_configuration`, `canonical_opus_configuration`, `canonical_vorbis_configuration` | One canonical decoder configuration per codec, whichever container carried it |
| DTS parser comparison | `reference_trace::dts::ParserOperation` (`reference-trace` feature) | Raw 14-bit source frames alongside the packets, clock and byte positions of FFmpeg's `dca` parser |

## Behaviour and limits

**Reference compatibility.** The crate-root types follow the specifications.
`compat::nal` reproduces FFmpeg's `mp4toannexb` packet receiver where a consumer needs
its result, and names the FFmpeg revision it reproduces. Comparison-only projections
need the non-default `reference-trace` feature.

**Input model.** Incremental readers own a core `InputCursor` and accept same-pool
windows of any size with the exact token, source and offset. An empty window is
starvation; a known length is not EOF. Truncation, transport failure and cancellation
are sticky errors, never a synthetic end. A new source or reset needs a new reader.
Record parsers take a complete pool-owned record (box payload, no size/type header);
sample jobs take one complete container sample.

**Source preservation.** Outputs keep exact source bytes, offsets, source ID and
generation. Raw and reserved fields stay readable; local violations are reported through
`syntax_issues()` bitsets rather than cleared. Absent and present-zero fields are
distinct (`None` vs `Some(0)`). Views share the pool-owned input, and a retained view
keeps the whole backing allocation charged.

**Resources.** All arithmetic is checked. Every operation has caller limits (bytes,
NAL/OBU/frame counts, input window) and charges work per byte or field. Allocation
pressure returns retryable `Backpressure` at the exact checkpoint; no partial output is
published. A `NalPacketSession` frames packets, records and outputs of at most
4 GiB − 1 bytes (`NalPacketLimits::MAXIMUM_SPAN`).

**Codec-specific limits.**

- NAL: payload bytes are opaque; no RBSP, SPS/PPS body or decoder-consistency check.
  The leading `zero_byte` rule is not enforced. `AnnexBWriter` never edits emulation
  prevention.
- avcC/hvcC parsers refuse `lengthSizeMinusOne = 2` (three-byte lengths) and unknown
  versions; the `compat::nal` receiver parsers accept them for the packet session only.
- `RawAnnexBReader` refuses data-partitioned, redundant-picture and extended AVC
  profiles and adds no timestamps. `resume_at` starts a fresh reader at an independently
  established access-unit boundary; resumed bytes must carry their own parameter sets.
- AV1: LEB128 up to 8 bytes and `u32::MAX`; `OneObu` needs an external length; AV1
  Annex-B (length-delimited) streams and sequence headers are not parsed.
- VP9: only vpcC version 1; no frame header or superframe parsing.
- AAC: other object types, unsupported extension modes and frequency indices 13/14 are
  refused. ADTS wrapping refuses channel configurations above 7. ADTS CRC is carried but
  not verified; multi-block and protected frames are not parsed.
- AC-3/E-AC-3: input must be aligned big-endian syncframes; no resync search,
  word-swapped or IEC 61937 input, and no CRC or audio-block check. bsid 9/10 and E-AC-3
  mixing definition 3 are refused. `sample_span()` is the coded duration of one frame,
  not a presentation time.
- DTS: canonical big-endian normal frames only in `DtsCoreFrameReader`; no resync or
  termination frames. HCRC is reported, not checked.

VVC inspection exposes the fixed two-byte header, including reserved values.
The Annex B iterator borrows NAL bytes from a complete buffer under an explicit
byte limit.

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
| MPEG audio | ISO/IEC 11172-3 §2.4.2.3, §2.4.3.1; ISO/IEC 13818-3 |
| FLAC, Opus, Vorbis | RFC 9639; RFC 7845 §5.1 and RFC 6716 §§3.1–3.2; Vorbis I §§4.2–4.3 |

Test fixtures: `fixtures/*.tsv` are bit-authored corpora with expected values from an
independent whole-buffer oracle. `fixtures/raw-annex-b/*.packets` hold the packet extents
and key flags FFmpeg's raw H.264/HEVC demuxers produce for the paired streams;
`hevc-eos.hevc` is `hevc-aud0.hevc` with an end of sequence before its second VPS and an
end of bitstream at the end.

Verification: the [packet-framing
harness](../../harness/features/packet-framing/README.md); results in
[docs/status.md](../../docs/status.md).
