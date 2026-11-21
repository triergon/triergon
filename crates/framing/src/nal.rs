//! NAL unit streams of H.264 and H.265: fixed headers, Annex B byte streams and
//! length-prefixed samples, parameter-set records, access units and packets.
//!
//! | Need | Entry points |
//! |---|---|
//! | NAL units of a buffer | [`AnnexBUnits`], [`start_code`], [`split_length_prefixed`], [`NalHeader::parse`] |
//! | NAL units of a stream, any chunking | [`NalReader`] with [`Framing`] |
//! | Access units of an elementary stream | [`RawAnnexBReader`] |
//! | Length-prefixed samples to Annex B packets | [`NalPacketSession`], [`NalPacketConfigWriter`] |
//! | Parameter sets of Annex B or declared configuration data | [`AnnexBConfigParser`], [`DeclaredConfigParser`] |
//! | One NAL unit behind a start code | [`AnnexBWriter`] |
//!
//! The codec-specific records, headers and type values are in [`avc`](crate::avc)
//! and [`hevc`](crate::hevc).

pub(crate) mod annexb;
mod config;
mod declared_config;
pub(crate) mod header;
pub(crate) mod packet;
mod packet_config;
mod raw_annex_b;
mod reader;
pub(crate) mod syntax;
mod units;

pub use annexb::{AnnexBOutput, AnnexBWriter};
pub use config::{
    AnnexBConfigParser, ConfigEncoding, ConfigInspection, ConfigLimits, Configuration, ParameterSet,
};
pub use declared_config::{
    DeclaredConfigArray, DeclaredConfigArrayView, DeclaredConfigLimits, DeclaredConfigNal,
    DeclaredConfigNalView, DeclaredConfigParser, DeclaredConfiguration,
};
pub use header::{Codec, NalHeader, ParameterSetKind};
pub use packet::{
    NalPacket, NalPacketConfiguration, NalPacketFields, NalPacketJob, NalPacketLimits,
    NalPacketOrigin, NalPacketOutput, NalPacketProgress, NalPacketReceiverErrorKind,
    NalPacketRejection, NalPacketSegment, NalPacketSession,
};
pub use packet_config::{NalPacketConfigOutput, NalPacketConfigRejection, NalPacketConfigWriter};
pub use raw_annex_b::{RawAccessUnit, RawAnnexBLimits, RawAnnexBReader};
pub use reader::{Framing, LengthSize, NalFacts, NalFraming, NalReader, NalUnit, ReaderLimits};
pub use syntax::{skip_hevc_profile_tier_level, skip_vui_to_timing};
pub use units::{AnnexBUnits, split_length_prefixed, start_code};
