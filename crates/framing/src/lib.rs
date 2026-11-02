//! Bounded NAL/OBU framing, AV1/VP9 records and AC-3/E-AC-3 source facts.
//! These development slices are not qualified O21.
//!
//! Payloads are opaque encoded bytes: no RBSP unescaping, access-unit
//! assembly, parameter-set injection, decoder, or display policy is applied.
//! See `README.md` for the exact configuration and validation boundary.
#![forbid(unsafe_code)]

mod ac3;
mod ac3_config;
mod annexb;
mod av1;
mod av1_config;
mod config;
mod declared_config;
mod dts_core;
mod dts_inspect;
mod dts_packing;
mod dts_source;
mod eac3_bsi;
mod header;
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
    DTS14_SYNC_PENDING, Dts14BoundaryEvidence, Dts14SyncFacts, Dts14SyncFrame, Dts14SyncLimits,
    Dts14SyncReader,
};

mod dts_normal;
mod dts_packed;
mod dts_packed_header;

pub use dts_normal::{
    DTS_NORMAL_PENDING, DtsCarrierTail, DtsExtentRelation, DtsNormalBoundary, DtsNormalFacts,
    DtsNormalLimits, DtsNormalSourceFrame, DtsNormalSourceReader,
};
pub use dts_packed::{DtsPackedLimits, DtsPackedPayload, DtsPackedPayloadJob, DtsPacking};
pub use dts_packed_header::{DtsPackedHeader, inspect_dts_header_bits};

mod avc_config;
pub use avc_config::{
    AvcConfigArray, AvcConfigArrayKind, AvcConfigFields, AvcConfigLimits, AvcConfigNal,
    AvcConfigParser, AvcConfigSource, AvcConfiguration,
};
