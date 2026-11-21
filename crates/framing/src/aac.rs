//! AAC: the AudioSpecificConfig and ADTS.
//!
//! | Need | Entry points |
//! |---|---|
//! | AudioSpecificConfig | [`AacAscParser`], [`AacAudioSpecificConfig`], [`AacAscFacts`] |
//! | The configuration a decoder is opened with | [`AacAscScopeParser`], [`decoder_configuration_view`], [`decoder_channel_view`] |
//! | ADTS header | [`inspect_adts_header`], [`AacAdtsHeader`] |
//! | ADTS frames of a complete buffer or a stream | [`AacAdtsParser`], [`AacAdtsStreamReader`], [`AacAdtsCarrierReader`] |
//! | Raw access units wrapped in ADTS | [`AacAdtsParameters`], [`AacAdtsWriter`] |

mod adts;
mod config;
mod config_views;

pub use adts::{
    AacAdtsCarrier, AacAdtsCarrierBoundary, AacAdtsCarrierLimits, AacAdtsCarrierReader,
    AacAdtsFrame, AacAdtsHeader, AacAdtsOutput, AacAdtsParameters, AacAdtsParser,
    AacAdtsStreamLimits, AacAdtsStreamReader, AacAdtsWriter, inspect_adts_header,
};
pub use config::{
    AacAscFacts, AacAscParser, AacAudioSpecificConfig, AacBitRange, AacConfigLimits,
    AacExtensionSignal, AacFrequency, AacGaSpecific, AacObjectType, AacPceElement,
    AacPceElementKind, AacPceFacts, AacProgramConfig, AacSource, fixed_channel_count,
    indexed_frequency,
};
pub use config_views::{
    AacAscScopeOutcome, AacAscScopeParser, AacAscScopeViews, AacDecoderChannelReason,
    AacDecoderChannelView, AacDecoderConfigDecision, AacDecoderConfigPolicy,
    AacDecoderConfigRefusal, decoder_channel_view, decoder_configuration_view,
};
