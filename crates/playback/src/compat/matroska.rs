//! Matroska AAC initialization against FFmpeg revision `d32b387f`.

use triergon_core::{Error, Result};
use triergon_framing::indexed_aac_frequency;
use triergon_matroska::{
    matroska::LegacyAacProfile,
    typed::{Configuration, InitializationOrigin},
};

/// Derive a two-byte AudioSpecificConfig for a legacy AAC-LC track with an
/// indexed sampling frequency and a channel code from 1 through 7. If the
/// track has initialization data or uses a different CodecID, return `None`.
/// The derived record is for the decoder feed and leaves the source facts intact.
///
/// # Errors
///
/// The track must exist. A legacy AAC-LC track without initialization must
/// declare a rate and channel code the AAC-LC ADTS feed can represent.
pub fn legacy_aac_lc_initialization(
    configuration: &Configuration,
    track: usize,
) -> Result<Option<[u8; 2]>> {
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
    if !init.data.is_empty() || init.origin != InitializationOrigin::InitialTrackEntry {
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
        .filter(|code| (1..=7).contains(code))
        .ok_or(Error::Unsupported("legacy Matroska AAC channel code"))?;
    // AAC-LC object type 2, the frequency index, the channel code, and three
    // zero GASpecificConfig flags.
    Ok(Some([
        0x10 | (frequency >> 1),
        ((frequency & 1) << 7) | (channels << 3),
    ]))
}
