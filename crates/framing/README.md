# triergon-framing

Packet framing and codec-configuration parsing for the format layer: H.264/HEVC
NAL units and records, AV1 OBUs, VP9 records, AAC, AC-3/E-AC-3 and DTS core.
Containers hand it record bytes and complete samples; it returns source-preserving
views, decoder-ready Annex-B/ADTS/temporal-unit output and typed refusals. It
depends on `triergon-core` and `triergon-bitstream`, does no I/O and does not decode
audio or video.

## Usage

Every operation takes a core `Budget` pool, a `WorkBudget` and a cancel flag, and
is driven by `step`. Record and packet jobs return `BuildStep::{Yield, Ready,
Complete}`; incremental readers return core `Step::{NeedInput, Yield, Ready, End}`
and are fed with `provide` and closed with `finish` or `fail`.

```rust
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_framing::{
    AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits, AvcConfigLimits, AvcConfigSource,
    AvcConfigViewParser, BuildStep,
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
let mut parser = AvcConfigViewParser::new(&pool, source, Bytes::copy_from_slice(&pool, &avcc)?, limits)?;
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
| Raw Annex-B access units | `RawAnnexBReader`, `RawAccessUnit` | Groups an elementary AVC/HEVC stream into access units (H.264 §7.4.1.2.3–4, H.265 §7.4.2.4.4); reports IDR/IRAP; keeps original start codes and padding |
| avcC | `AvcConfigParser`, `AvcConfigViewParser` | Full record: fixed fields, SPS/PPS/SPS-ext arrays, high-profile extension, trailing bytes |
| hvcC | `HvcConfigParser`, `HvcConfigViewParser`, `inspect_hvc_config_prefix` | Version 1: 23-byte fixed prefix, all NAL arrays in order (unknown/duplicate/empty arrays kept) |
| Sample to Annex-B | `AvcAnnexBJob`, `HevcAnnexBJob` | Four-byte start codes; prepends record SPS/PPS (VPS/SPS/PPS) before IDR/IRAP; zero-copy segments |
| Record to Annex-B | `HvcAnnexBWriter`, `HvcDecoderConfigWriter`, `AnnexBWriter` | All hvcC NALs in order, or the decoder subset (types 32–34, 39, 40); single NAL with `00 00 00 01` |
| FFmpeg-compatible BSF | `NalPacketSession`, `NalPacketConfigWriter`, `compat::nal` | Stateful `h264_mp4toannexb`/`hevc_mp4toannexb` packet behaviour, including in-band parameter-set tracking and AVC `NEW_EXTRADATA` |
| Other config inputs | `AnnexBConfigParser`, `DeclaredConfigParser`, `Configuration::declared_record` | Annex-B extradata framing; caller-declared array/NAL layouts; opaque record retention |
| AV1 | `Av1ObuReader`, `Av1ConfigParser`, `Av1TemporalUnitJob` | Low-overhead or single-OBU framing with exact LEB128; av1C v1 fields and config OBUs; container sample to decoder temporal unit |
| VP9 | `Vp9ConfigParser` | vpcC v1 fields (profile, level, bit depth, chroma, range, CICP) |
| AAC | `AacAscParser`, `AacAscScopeParser`, `aac_decoder_configuration_view` | AudioSpecificConfig for AAC-LC (2), SBR (5), PS (29): frequency, channels 1–7, GASpecificConfig, PCE, `0x2b7`/`0x548` sync extensions |
| ADTS | `inspect_aac_adts_header`, `AacAdtsParser`, `AacAdtsWriter`, `AacAdtsStreamReader`, `AacAdtsCarrierReader` | 56-bit header facts; single-block AAC-LC frames; ADTS wrapping of raw access units (`AacAdtsParameters::from_asc`); aligned ADTS streams |
| AC-3 / E-AC-3 | `probe_ac3_header`, `Ac3FrameReader`, `Ac3ConfigParser`, `Eac3BsiParser` | Header facts (bsid 0–8, 11–16), aligned syncframe reader, dac3/dec3 records, complete E-AC-3 BSI |
| DTS core | `probe_dts_core_header`, `DtsCoreFrameReader`, `DtsCorePacket`, `DtsHeaderInspector` | Normal core header (all fields, optional HCRC), FSIZE+1 frame acquisition, container-packet core extraction |
| DTS packings | `Dts14BitUnpacker`, `DtsPackedPayloadJob`, `Dts14FrameReader`, `Dts14CarrierReader`, `DtsNormalSourceReader` | 16-bit LE and 14-bit BE/LE word packings; experimental 14-bit stream acquisition |
| DTS parser comparison | `reference_trace::dts::ParserOperation` (`reference-trace` feature) | Raw 14-bit source frames alongside the packets, clock and byte positions of FFmpeg's `dca` parser |

## Behaviour and limits

**Reference compatibility.** The crate-root types follow the specifications.
`compat::nal` reproduces FFmpeg where a consumer needs FFmpeg's result: it accepts
configuration records as the `mp4toannexb` packet receiver does, and names the
FFmpeg revision it reproduces. Comparison-only projections are compiled only with
the non-default `reference-trace` feature: the reference's return codes, and under
`reference_trace::dts` the packets, clock and byte positions of FFmpeg's `dca`
parser over raw 14-bit DTS.

**Input model.** Incremental readers own a core `InputCursor` and accept same-pool
windows of any size with the exact token, source and offset. An empty window is
starvation; a known length is not EOF. Truncation, transport failure and
cancellation are sticky errors, never a synthetic end. A new source or reset needs
a new reader. Record parsers take a complete pool-owned record (box payload, no
size/type header); sample jobs take one complete container sample.

**Source preservation.** Outputs keep exact source bytes, offsets, source ID and
generation. Raw and reserved fields stay readable; local violations are reported
through `syntax_issues()` bitsets rather than cleared. Absent and present-zero
fields are distinct (`None` vs `Some(0)`). Views share the pool-owned input; a
retained view keeps the whole backing allocation charged. Small AVC records can
reuse the checked offsets from the validation pass when the remaining work
budget covers publishing the descriptors.

**Resources.** All arithmetic is checked. Every operation has caller limits
(bytes, NAL/OBU/frame counts, input window). Work is charged per byte or field;
one unit always progresses and zero units consume nothing. Copies are at most
4096 bytes between cancel checks. Allocation pressure returns retryable
`Backpressure` at the exact checkpoint; no partial output is published.
Stateful packet jobs allocate segment storage as needed and share the original
payload for passthrough packets.

**Codec-specific limits.**

- NAL: payload bytes are opaque. No RBSP, SPS/PPS body or decoder-consistency
  check. Annex-B termination follows H.264 B.2 / H.265 B.3; the leading `zero_byte`
  rule is not enforced. `AnnexBWriter` never edits emulation prevention.
- avcC/hvcC parsers refuse `lengthSizeMinusOne = 2` (three-byte lengths) and
  unknown versions. The `compat::nal` receiver parsers accept them for the packet
  session only.
- `RawAnnexBReader` refuses data-partitioned, redundant-picture and extended AVC
  profiles. It adds no timestamps.
- AV1: LEB128 up to 8 bytes and `u32::MAX`. `OneObu` needs an external length;
  AV1 Annex-B (length-delimited) streams are not parsed. Sequence headers are not
  parsed. `Av1TemporalUnitJob` adds a missing temporal delimiter, inserts av1C
  config OBUs at random access when the sample has no sequence header, and writes
  a size field for a size-less final OBU.
- VP9: only vpcC version 1. No frame header or superframe parsing.
- AAC: other object types, unsupported extension modes and frequency indices
  13/14 are refused. ADTS CRC is carried but not verified; multi-block and
  protected frames are not parsed. The writer obeys the 8191-byte frame limit.
- AC-3/E-AC-3: input must be aligned big-endian syncframes. There is no resync
  search, no word-swapped or IEC 61937 input, and no CRC or audio-block check.
  bsid 9/10 and E-AC-3 mixing definition 3 are refused. `sample_span()` is the
  coded duration of one frame, not a presentation time.
- DTS: canonical big-endian normal frames only in `DtsCoreFrameReader`; no
  resync, termination frames or DTS-HD extension parsing. HCRC is reported, not
  checked. `DtsCorePacket` normalises word-swapped and 14-bit cores into caller
  storage and leaves extension bytes in the source packet.

## Conformance

Implemented from these specifications:

| Area | Reference |
|---|---|
| H.264 NAL / Annex B / AU | ITU-T H.264 (08/2024) §7.3.1, §7.4.1 Table 7-1, §§7.3.2.1.1, 7.3.2.2, 7.3.3, 7.4.1.2.3–4, Annex B |
| HEVC NAL / Annex B / AU | ITU-T H.265 (07/2024) §7.3.1.2, §7.4.2 Table 7-1, §7.4.2.4.4, Annex B |
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
  raw H.264/HEVC demuxers produce for the paired streams.
- The `examples/` trace programs (`nal_packet_receiver_trace`,
  `dts-operation-trace`, `dts-source-observer`) emit JSON records that the
  harness compares with FFmpeg's BSF and DCA parser output. The first two need
  `--features reference-trace`.

Per-module parity, runtime and memory results are in
[docs/status.md](../../docs/status.md); the gates and harness are described in
[docs/quality.md](../../docs/quality.md).
