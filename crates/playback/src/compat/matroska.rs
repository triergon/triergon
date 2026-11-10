//! Matroska AAC initialization against FFmpeg revision `d32b387f`.

use triergon_core::{Error, Result};
use triergon_framing::{AacAdtsParameters, indexed_aac_frequency};
use triergon_matroska::{
    matroska::LegacyAacProfile,
    typed::{Configuration, InitializationOrigin},
};

/// Derive a two-byte AudioSpecificConfig for a legacy AAC-LC track with an
/// indexed sampling frequency and one through six channels. If the track
/// carries initialization or uses a different CodecID, return `None`.
/// The derived record is for the decoder feed and leaves the source facts intact.
///
/// # Errors
///
/// The track must exist. A legacy AAC-LC track without initialization must
/// declare a rate and channel count the AAC-LC ADTS feed can represent.
pub fn legacy_aac_lc_initialization(
    configuration: &Configuration,
    track: usize,
) -> Result<Option<[u8; 2]>> {
    Ok(
        legacy_aac_lc_parameters(configuration, track)?.map(|params| {
            // AAC-LC object type 2, the frequency index, the channel code, and
            // three zero GASpecificConfig flags.
            [
                0x10 | (params.frequency_index >> 1),
                ((params.frequency_index & 1) << 7) | (params.channel_configuration << 3),
            ]
        }),
    )
}

pub(crate) fn legacy_aac_lc_parameters(
    configuration: &Configuration,
    track: usize,
) -> Result<Option<AacAdtsParameters>> {
    let descriptor = configuration
        .tracks()
        .get(track)
        .ok_or(Error::Invalid("Matroska AAC track index"))?;
    if descriptor.legacy_aac_profile() != Some(LegacyAacProfile::LowComplexity) {
        return Ok(None);
    }
    let init = configuration
        .initialization(track)
        .ok_or(Error::Invalid("missing Matroska AAC initialization"))?;
    if descriptor.has_codec_private()
        || !init.data.is_empty()
        || init.origin != InitializationOrigin::InitialTrackEntry
    {
        return Ok(None);
    }
    let frequency = (0..13)
        .find(|&index| {
            indexed_aac_frequency(index)
                .is_some_and(|hz| f64::from(hz).to_bits() == descriptor.sample_rate.to_bits())
        })
        .ok_or(Error::Unsupported("legacy Matroska AAC sampling frequency"))?;
    let channels = u8::try_from(descriptor.channels)
        .ok()
        // The legacy track carries a count. Code 7 specifies an eight-channel
        // layout, which cannot be inferred from a count alone.
        .filter(|count| (1..=6).contains(count))
        .ok_or(Error::Unsupported("legacy Matroska AAC channel layout"))?;
    Ok(Some(AacAdtsParameters {
        mpeg2_id: false,
        frequency_index: frequency,
        channel_configuration: channels,
        buffer_fullness: 0x7ff,
        private_bit: false,
        original_copy: false,
        home: false,
        copyright_id_bit: false,
        copyright_id_start: false,
    }))
}
