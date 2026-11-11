//! Decoding the packets of one container audio track.
use std::sync::atomic::AtomicBool;
use triergon_codec::{AudioDecoder, AudioFrame, Budget, DecodeFailure, DownmixLevels};
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_core::{BudgetVec, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AacAscFacts, AacAscParser, AacAudioSpecificConfig, AacConfigLimits, AacSource, BuildStep,
    aac_fixed_channel_count, indexed_aac_frequency,
};

use crate::compat::aac::Policy;
use crate::{AudioCodec, Discard, audio_codec, audio_configuration, audio_decoder_in};

/// The decoder of one container audio track. It chooses the decoder from the track's
/// codec and configuration, and drops the decoded samples each packet's container marks
/// for discarding ([`Discard`]), and at least the decoder's own start delay
/// ([`AudioDecoder::delay`]) at the start of the stream.
pub struct TrackDecoder {
    codec: AudioCodec,
    decoder: Box<dyn AudioDecoder>,
    /// Samples of the decoder's start delay not discarded yet.
    delay: usize,
}

impl core::fmt::Debug for TrackDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TrackDecoder")
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

impl TrackDecoder {
    /// The pool [`TrackDecoder::new`] creates: the largest default of the decoders it
    /// chooses from.
    pub const DEFAULT_STORAGE_BYTES: usize = 32 << 20;

    /// The decoder for `track`; refused when this crate does not decode its codec or its
    /// configuration is invalid.
    pub fn new(track: &dyn TrackInfo) -> Result<TrackDecoder, DecodeFailure> {
        let budget = Budget::new(Self::DEFAULT_STORAGE_BYTES).map_err(DecodeFailure::from_error)?;
        Self::new_in(track, &budget)
    }

    /// [`TrackDecoder::new`] charging the configuration and the decoder to `budget`.
    pub fn new_in(track: &dyn TrackInfo, budget: &Budget) -> Result<TrackDecoder, DecodeFailure> {
        let codec = audio_codec(track.codec())
            .ok_or_else(|| DecodeFailure(format!("unsupported codec {:?}", track.codec())))?;
        let mut configuration = BudgetVec::new(budget);
        audio_configuration(codec, track.configuration(), &mut configuration)
            .map_err(DecodeFailure::from_error)?;
        let decoder = audio_decoder_in(codec, &configuration, budget)?;
        Ok(TrackDecoder {
            codec,
            delay: decoder.delay(),
            decoder,
        })
    }

    /// Decode raw AAC-LC from the selected sample description's AudioSpecificConfig.
    /// The caller must use a decoder configured for each packet's description.
    /// Use [`Self::new_in`] for ADTS-framed packets.
    pub fn aac_access_units_in(
        configuration: &[u8],
        budget: &Budget,
    ) -> Result<Self, DecodeFailure> {
        Self::aac_access_units_with_layout_in(configuration, Policy::Specification, budget)
    }

    /// Decode configured AAC-LC with a speaker mapping for fixed layouts.
    /// An explicit program layout uses the element tags in AudioSpecificConfig.
    pub fn aac_access_units_with_layout_in(
        configuration: &[u8],
        layout: Policy,
        budget: &Budget,
    ) -> Result<Self, DecodeFailure> {
        let parsed = parse_aac_access_unit_config(configuration, budget)?;
        let decoder: Box<dyn AudioDecoder> = if let Some(program) = parsed.program_config() {
            validate_raw_aac_lc(parsed.facts())?;
            let bits = program.facts().source_bits;
            if !bits.start_bit.is_multiple_of(8) || !bits.bit_count.is_multiple_of(8) {
                return Err(DecodeFailure(
                    "unaligned raw AAC program configuration".into(),
                ));
            }
            let start = bits.start_bit / 8;
            let end = start + bits.bit_count / 8;
            let source = parsed
                .source_bytes()
                .as_ref()
                .get(start..end)
                .ok_or_else(|| DecodeFailure("raw AAC program extent".into()))?;
            let decoder = triergon_codec_aac::RawLcDecoder::with_program_config_in(
                parsed.facts().core_frequency.index,
                source,
                budget,
            )
            .map_err(DecodeFailure::from_error)?;
            Box::new(decoder)
        } else {
            AacAccessUnitConfig::from_asc(parsed.facts())?.decoder_with_layout_in(layout, budget)?
        };
        Ok(Self {
            codec: AudioCodec::Aac,
            delay: decoder.delay(),
            decoder,
        })
    }

    /// The codec being decoded.
    pub fn codec(&self) -> AudioCodec {
        self.codec
    }

    /// Replace `out` with the presented output of `packet`.
    pub fn decode(
        &mut self,
        packet: &impl PacketInfo,
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        let levels = self.decoder.decode(packet.payload(), out)?;
        Discard::of(packet).apply(out, &mut self.delay);
        Ok(levels)
    }

    /// End of stream: replace `out` with the samples the decoder still holds back.
    pub fn flush(&mut self, out: &mut AudioFrame) -> Result<(), DecodeFailure> {
        self.decoder.flush(out)
    }

    /// Discontinuity: drop the decoder's signal history.
    pub fn reset(&mut self) {
        self.decoder.reset();
    }

    /// The decoder, for a session that paces it ([`crate::AudioSession::load`]).
    pub fn into_decoder(self) -> Box<dyn AudioDecoder> {
        self.decoder
    }
}

/// The fixed configuration of raw AAC-LC packets in a container track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacAccessUnitConfig {
    frequency_index: u8,
    channel_configuration: u8,
}

impl AacAccessUnitConfig {
    /// Select indexed rates, 1024 core samples and fixed layouts up to eight channels
    /// from parsed AudioSpecificConfig facts.
    pub fn from_asc(facts: AacAscFacts) -> Result<Self, DecodeFailure> {
        validate_raw_aac_lc(facts)?;
        if aac_fixed_channel_count(facts.channel_configuration).is_none() {
            return Err(DecodeFailure("unsupported raw AAC-LC configuration".into()));
        }
        Ok(Self {
            frequency_index: facts.core_frequency.index,
            channel_configuration: facts.channel_configuration,
        })
    }

    /// Build the decoder with the default track-decoder storage budget.
    pub fn decoder(self) -> Result<Box<dyn AudioDecoder>, DecodeFailure> {
        let budget =
            Budget::new(TrackDecoder::DEFAULT_STORAGE_BYTES).map_err(DecodeFailure::from_error)?;
        self.decoder_in(&budget)
    }

    /// Build a decoder for one raw AAC-LC access unit per packet.
    pub fn decoder_in(self, budget: &Budget) -> Result<Box<dyn AudioDecoder>, DecodeFailure> {
        self.decoder_with_layout_in(Policy::Specification, budget)
    }

    /// Build the configured decoder with an explicit speaker mapping.
    pub fn decoder_with_layout_in(
        self,
        layout: Policy,
        budget: &Budget,
    ) -> Result<Box<dyn AudioDecoder>, DecodeFailure> {
        let decoder = triergon_codec_aac::RawLcDecoder::with_layout_in(
            self.frequency_index,
            self.channel_configuration,
            layout,
            budget,
        )
        .map_err(DecodeFailure::from_error)?;
        Ok(Box::new(decoder))
    }
}

fn validate_raw_aac_lc(facts: AacAscFacts) -> Result<(), DecodeFailure> {
    if facts.outer_object_type.code != 2
        || facts.core_object_type.code != 2
        || facts.core_frequency.explicit_hz.is_some()
        || indexed_aac_frequency(facts.core_frequency.index) != Some(facts.core_frequency.hz)
        || facts.ga.frame_length_flag
        || facts.ga.core_coder_delay.is_some()
        || facts.ga.extension_flag3 == Some(true)
        || facts.sbr.is_present()
        || facts.ps.is_present()
    {
        return Err(DecodeFailure("unsupported raw AAC-LC configuration".into()));
    }
    Ok(())
}

fn parse_aac_access_unit_config(
    configuration: &[u8],
    budget: &Budget,
) -> Result<AacAudioSpecificConfig, DecodeFailure> {
    let limits = AacConfigLimits {
        maximum_record_bytes: 4096,
        maximum_pce_elements: 64,
        maximum_comment_bytes: 255,
    };
    if configuration.len() > limits.maximum_record_bytes {
        return Err(DecodeFailure::from_error(Error::Limit {
            requested: configuration.len(),
            maximum: limits.maximum_record_bytes,
        }));
    }
    let bytes = Bytes::copy_from_slice(budget, configuration).map_err(DecodeFailure::from_error)?;
    let mut parser = AacAscParser::new(
        budget,
        AacSource {
            source: SourceId(0),
            generation: 0,
            offset: 0,
        },
        bytes,
        limits,
    )
    .map_err(DecodeFailure::from_error)?;
    let BuildStep::Ready(parsed) = parser
        .step(&mut WorkBudget::new(u64::MAX), &AtomicBool::new(false))
        .map_err(DecodeFailure::from_error)?
    else {
        return Err(DecodeFailure("missing AAC AudioSpecificConfig".into()));
    };
    Ok(parsed)
}
