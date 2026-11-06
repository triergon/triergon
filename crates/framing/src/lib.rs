#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`.

mod dts_operation;
pub use dts_operation::{
    DtsD32ClockDisposition, DtsD32ClockProjection, DtsFactAssociation, DtsFactAssociationJob,
    DtsOperation, DtsOperationContext, DtsOperationEvent, DtsOperationLane, DtsOperationLimits,
    DtsOperationStep, DtsOperationTerminal, DtsParserPacket,
};

mod nal_packet;
pub use nal_packet::{
    NalPacket, NalPacketCompactJob, NalPacketCompactOutput, NalPacketConfiguration,
    NalPacketFields, NalPacketJob, NalPacketLimits, NalPacketOrigin, NalPacketOutput,
    NalPacketProgress, NalPacketRangeSegment, NalPacketReceiverErrorKind, NalPacketRejection,
    NalPacketSegment, NalPacketSession, NalPacketViewConfiguration, NalPacketViewJob,
};

mod ac3;
mod ac3_config;
mod annexb;
mod av1;
mod av1_config;
mod av1_temporal_unit;
mod avc_picture;
mod config;
mod declared_config;
mod dts_core;
mod dts_inspect;
mod dts_packing;
mod dts_source;
mod eac3_bsi;
mod header;
mod raw_annex_b;
mod reader;
mod record_input;
mod vp9_config;

pub use ac3::{
    Ac3Family, Ac3Frame, Ac3FrameDetails, Ac3FrameFacts, Ac3FrameHeader, Ac3FrameLimits,
    Ac3FrameReader, Ac3HeaderIssues, Ac3StreamSelection, Eac3DualMono, probe_ac3_header,
};
pub use ac3_config::{
    Ac3ConfigIssues, Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration,
    Dac3Configuration, Dac3Fields, Dec3Configuration, Dec3Substream,
};
pub use annexb::{AnnexBOutput, AnnexBWriter, BuildStep};
pub use av1::{
    Av1Leb128, Av1Obu, Av1ObuExtension, Av1ObuFacts, Av1ObuFraming, Av1ObuHeader,
    Av1ObuHeaderIssues, Av1ObuLimits, Av1ObuReader,
};
pub use av1_config::{
    Av1ConfigFields, Av1ConfigLimits, Av1ConfigParser, Av1ConfigSyntaxIssues, Av1Configuration,
};
pub use av1_temporal_unit::{
    Av1TemporalUnit, Av1TemporalUnitConfig, Av1TemporalUnitJob, Av1TemporalUnitLimits,
};
pub use avc_access_unit::{AvcAnnexBAccessUnit, AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits};
pub use config::{
    AnnexBConfigParser, ConfigEncoding, ConfigInspection, ConfigLimits, Configuration, ParameterSet,
};
pub use declared_config::{
    DeclaredConfigArray, DeclaredConfigArrayView, DeclaredConfigLimits, DeclaredConfigNal,
    DeclaredConfigNalView, DeclaredConfigParser, DeclaredConfiguration,
};
pub use dts_core::{
    DtsCoreFrame, DtsCoreFrameFacts, DtsCoreFrameLimits, DtsCoreFrameReader, DtsCoreHeader,
    probe_dts_core_header,
};
mod dts_core_packet;
pub use dts_core_packet::DtsCorePacket;
pub use dts_inspect::{
    DtsCoreField, DtsHeaderInspector, DtsInspection, DtsInspectionIdentity, DtsInspectionIssues,
    DtsInspectionSourceStatus, DtsInspectionStep, DtsInspectionViews, FieldObservation,
};
pub use dts_packing::{
    Dts14BitPayload, Dts14BitUnpacker, DtsPackedBitLocation, DtsPackingLimits, DtsWordOrder,
};
pub use dts_source::DtsSourceIdentity;
pub use eac3_bsi::{
    Eac3BitRange, Eac3BitSpan, Eac3Bsi, Eac3BsiFacts, Eac3BsiLimits, Eac3BsiParser, Eac3Trailer,
};
pub use header::{Codec, NalHeader, ParameterSetKind};
pub use reader::{Framing, LengthSize, NalFacts, NalFraming, NalReader, NalUnit, ReaderLimits};
pub use vp9_config::{
    Vp9ConfigContext, Vp9ConfigFields, Vp9ConfigLimits, Vp9ConfigParser, Vp9ConfigSyntaxIssues,
    Vp9Configuration,
};

mod dts14_sync;
pub use dts14_sync::{
    Dts14BoundaryEvidence, Dts14SyncFacts, Dts14SyncFrame, Dts14SyncLimits, Dts14SyncReader,
};

mod dts_normal;
mod dts_packed;
mod dts_packed_header;

pub use dts_normal::{
    DtsCarrierTail, DtsExtentRelation, DtsNormalBoundary, DtsNormalFacts, DtsNormalLimits,
    DtsNormalSourceFrame, DtsNormalSourceReader,
};
pub use dts_packed::{DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob, DtsPacking};
pub use dts_packed_header::{DtsPackedHeader, inspect_dts_header_bits};

mod avc_access_unit;
mod avc_config;
pub use avc_config::{
    AvcConfigArray, AvcConfigArrayKind, AvcConfigFields, AvcConfigLimits, AvcConfigNal,
    AvcConfigParser, AvcConfigSource, AvcConfiguration,
};

mod dts14_frame;
pub use dts14_frame::{Dts14Frame, Dts14FrameReader};
mod dts14_carrier;
pub use dts14_carrier::{
    Dts14CarrierBoundary, Dts14CarrierLimits, Dts14CarrierPacket, Dts14CarrierProvision,
    Dts14CarrierReader, Dts14CarrierSyntax,
};
mod dts14_parser_view;
pub use dts14_parser_view::{
    DTS14_D32_POSITION_PROFILE, Dts14D32ParserHints, Dts14ParserHintOrigin,
};

mod hevc_access_unit;
pub use hevc_access_unit::{
    HevcAnnexBAccessUnit, HevcAnnexBConfig, HevcAnnexBJob, HevcAnnexBLimits,
};

mod hevc_config;
pub use hevc_config::{
    HvcConfigArray, HvcConfigFields, HvcConfigLimits, HvcConfigNal, HvcConfigParser,
    HvcConfigSource, HvcConfiguration, inspect_hvc_config_prefix,
};

mod aac_config;
pub use aac_config::{
    AacAscFacts, AacAscParser, AacAudioSpecificConfig, AacBitRange, AacConfigLimits,
    AacExtensionSignal, AacFrequency, AacGaSpecific, AacObjectType, AacPceElement,
    AacPceElementKind, AacPceFacts, AacProgramConfig, AacSource, indexed_aac_frequency,
};

mod aac_adts;
pub use aac_adts::{
    AacAdtsFrame, AacAdtsHeader, AacAdtsOutput, AacAdtsParameters, AacAdtsParser, AacAdtsWriter,
    inspect_aac_adts_header,
};
pub use aac_adts::{AacAdtsStreamLimits, AacAdtsStreamReader};

mod hevc_config_annexb;
pub use hevc_config_annexb::{HvcAnnexBOutput, HvcAnnexBWriter};

mod hevc_config_decoder;
pub use hevc_config_decoder::{
    HvcDecoderConfigOutput, HvcDecoderConfigRefusal, HvcDecoderConfigWriter,
};
mod aac_config_views;
pub use aac_config_views::{
    AacAscScopeOutcome, AacAscScopeParser, AacAscScopeViews, AacDecoderChannelReason,
    AacDecoderChannelView, aac_decoder_channel_view,
};

pub use aac_adts::{
    AacAdtsCarrier, AacAdtsCarrierBoundary, AacAdtsCarrierLimits, AacAdtsCarrierReader,
};

pub use aac_config_views::{
    AacDecoderConfigDecision, AacDecoderConfigPolicy, AacDecoderConfigRefusal,
    aac_decoder_configuration_view,
};

mod nal_packet_config;
pub use nal_packet_config::{
    NalPacketConfigOutput, NalPacketConfigRejection, NalPacketConfigWriter,
};

mod nal_observed_config;
pub use nal_observed_config::{
    NalAvcUpdateRejection, NalObservedAvcConfigParser, NalObservedAvcConfiguration,
    NalObservedHevcConfigParser, NalObservedHevcConfiguration,
};

// Compact configuration inspection and retained exports.
pub use avc_config::{
    AvcConfigArrayRef, AvcConfigNalRef, AvcConfigViewParser, AvcConfigurationView,
    RetainedAvcConfigArray, RetainedAvcConfigNal,
};
pub use hevc_config::{
    HvcConfigArrayRef, HvcConfigNalRef, HvcConfigViewParser, HvcConfigurationView,
    RetainedHvcConfigArray, RetainedHvcConfigNal,
};
pub use nal_observed_config::{
    NalObservedAvcConfigViewParser, NalObservedAvcConfigurationView,
    NalObservedHevcConfigViewParser, NalObservedHevcConfigurationView,
};

pub use raw_annex_b::{RawAccessUnit, RawAnnexBLimits, RawAnnexBReader};
