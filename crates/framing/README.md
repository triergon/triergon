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

The AVC and HEVC record parsers, their compatibility wrappers and the packet job also
offer `step_into`, which publishes the output into an empty caller-provided slot and
returns `SlotProgress`. An occupied slot is refused without advancing the job; `step`
has the same progress and errors.

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

## Features

| Feature | Default | Effect |
|---|---|---|
| `reference-trace` | off | `reference_trace::dts`, FFmpeg's DTS parser model, and the accessors that report the return codes FFmpeg's packet receivers give |

## Supported

| Module | Entry points | Output |
|---|---|---|
| `nal` | `NalReader`, `AnnexBUnits`, `RawAnnexBReader`, `AnnexBConfigParser`, `DeclaredConfigParser` | AVC/HEVC NAL units, raw access units and declared configurations with original bytes and offsets |
| `avc` | `AvcConfigParser`, `AvcAnnexBJob`, `read_sps`, `read_pps`, `read_slice_header` | avcC records, Annex B samples and validated SPS, PPS and slice syntax |
| `hevc` | `HvcConfigParser`, `HevcAnnexBJob`, `HvcAnnexBWriter`, `HvcDecoderConfigWriter`, `read_sps_prefix` | hvcC records, Annex B samples and parameter-set prefixes |
| `compat::nal` | `NalPacketSession`, `NalPacketConfigWriter`, `ReceiverAvcConfigParser`, `ReceiverHevcConfigParser` | Annex B packets and configuration as FFmpeg's `mp4toannexb` filters frame them, with in-band parameter sets |
| `picture_output` | `DecodedPictures`, `order::{AvcOrder, HevcOrder}`, `levels::{max_dpb_mbs, max_dpb_frames}` | Picture order, marking and bounded output for H.264 and H.265 |
| `av1` | `Av1ObuReader`, `Av1ConfigParser`, `Av1TemporalUnitJob` | Low-overhead or externally sized OBUs, av1C and temporal units |
| `vp9` | `Vp9ConfigParser` | vpcC profile, level, bit depth, range and CICP |
| `aac` | `AacAscParser`, `AacAscScopeParser`, `AacAdtsParser`, `AacAdtsWriter`, `AacAdtsStreamReader`, `AacAdtsCarrierReader` | AudioSpecificConfig and bounded single-block ADTS framing |
| `ac3` | `probe_header`, `Ac3FrameReader`, `Ac3ConfigParser`, `Eac3BsiParser` | Aligned AC-3 and E-AC-3 frames, dac3/dec3 records and E-AC-3 BSI |
| `dts` | `probe_core_header`, `probe_frame_extent`, `probe_extension_substream_extent`, `DtsCoreFrameReader`, `DtsHeaderInspector` | Core headers, complete core acquisition and declared core and extension extents |
| `dts` packed | `DtsPackedPayloadJob`, `Dts14CarrierReader`, `DtsNormalSourceReader` | Canonical header and payload from packed DTS words |
| `flac` | `stream_info`, `metadata_blocks`, `FlacFrameHeader`, `canonical_configuration` | STREAMINFO, metadata, frame headers and canonical FLAC configuration |
| `opus` | `OpusHead`, `packet_samples`, `canonical_configuration` | Identification, TOC duration and canonical Opus configuration |
| `vorbis` | `VorbisHeaders`, `VorbisIdentification`, `VorbisModes`, `canonical_configuration` | Xiph-laced headers, modes and canonical Vorbis configuration |
| `mpeg_audio` | `MpegAudioHeader`, `info_frame` | Layers I–III frame headers with MPEG-2.5 rates, Xing, Info and VBRI |
| `reference_trace::dts` | `ParserOperation` | Source frame and demuxer parser packet, clock and position comparisons |

## Behaviour and limits

**Reference compatibility.** The format modules follow the specifications.
`compat::nal` reproduces FFmpeg's `mp4toannexb` packet receiver where a consumer needs
its result, and names the FFmpeg revision it reproduces.

**Input model.** Incremental readers own a core `InputCursor` and accept same-pool
windows of any size with the exact token, source and offset. An empty window is
starvation; a known length is not EOF. Truncation, transport failure and cancellation
are sticky errors, never a synthetic end. The ADTS stream and carrier readers and the
DTS normal, 14-bit sync and 14-bit carrier readers restart on the same source at an
offset with `reset`, under a new token generation; every other reader needs a new
reader for a new source or position. Record parsers take a complete pool-owned record (box payload, no size/type header);
sample jobs take one complete container sample.

**Source preservation.** Outputs keep exact source bytes, offsets, source ID and
generation. Raw and reserved fields stay readable; local violations are reported through
`syntax_issues()` bitsets rather than cleared. Absent and present-zero fields are
distinct (`None` vs `Some(0)`). Views share the pool-owned input, and a retained view
keeps the whole backing allocation charged.

**Resources.** All arithmetic is checked. Every operation has caller limits (bytes,
NAL/OBU/frame counts, input window) and charges work per byte or field. Allocation
pressure returns retryable `Backpressure` at the exact checkpoint; no partial output is
published. A `compat::nal::NalPacketSession` frames packets, records and outputs of at
most 4 GiB − 1 bytes (`NalPacketLimits::MAXIMUM_SPAN`).

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

## Conformance

Implemented from these specifications:

| Area | Reference |
|---|---|
| H.264 NAL / Annex B / AU | ITU-T H.264 (08/2024) §7.3.1, §7.4.1 Table 7-1, §§7.3.2.1.1, 7.3.2.2, 7.3.3, 7.4.1.2.3–4, Annex B |
| HEVC NAL / Annex B / AU | ITU-T H.265 (07/2024) §7.3.1.2, §7.4.2 Table 7-1, §7.4.2.4.4, Annex B |
| Picture output | ITU-T H.264 (08/2024) §8.2.1, §8.2.5.3–4, §A.3.1 Table A-1, Annex C.4.4–C.4.5; ITU-T H.265 (07/2024) §8.1.3, §8.3.1–8.3.2, Annex C.5.2 |
| avcC | ISO/IEC 14496-15:2010 with the 2006 FRExt amendment |
| hvcC | ISO/IEC 14496-15 §8.3.3.1 (`HEVCDecoderConfigurationRecord`) |
| AAC ASC / ADTS | ISO/IEC 14496-3 §1.6.2.1 (`AudioSpecificConfig`), §1.A.2.2 (ADTS) |
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

The [packet-framing comparison](../../harness/features/packet-framing/README.md) and
the codec configuration and parser view features compare these outputs with FFmpeg;
results are in [docs/status.md](../../docs/status.md).
