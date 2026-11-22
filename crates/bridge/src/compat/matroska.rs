//! Matroska AAC initialization against FFmpeg revision `d32b387f`.

use triergon_core::{Error, Result};
use triergon_framing::aac::{self, AacAdtsParameters};
use triergon_matroska::{Configuration, InitializationOrigin, LegacyAacProfile};

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
    if !descriptor.has_audio_settings() {
        return Err(Error::Unsupported(
            "legacy Matroska AAC needs Audio settings",
        ));
    }
    let frequency = (0..13)
        .find(|&index| {
            aac::indexed_frequency(index)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use triergon_core::{
        Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
    };
    use triergon_matroska::{Event, Limits, Reader};

    use super::*;

    const LC: &[u8] = include_bytes!("../../../playback/tests/fixtures/mkv-feed/aac-mpeg4-lc.mkv");

    fn configuration(input: &[u8]) -> Configuration {
        let pool = Budget::new(2 << 20).unwrap();
        let caps = SourceCapabilities {
            length: Some(input.len() as u64),
            random_access: true,
        };
        let mut reader = Reader::new(&pool, SourceId(1), caps, Limits::default(), 4096).unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..1024 {
            match reader.step(&mut WorkBudget::new(65536), &cancel).unwrap() {
                Step::Ready(Event::Configuration(configuration)) => return configuration,
                Step::NeedInput(request) => {
                    let at = request.offset as usize;
                    let len = request.maximum.min(input.len() - at);
                    let bytes = Bytes::copy_from_slice(&pool, &input[at..at + len]).unwrap();
                    let window =
                        InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap();
                    reader.provide(&window).unwrap();
                }
                Step::Ready(_) | Step::Yield => (),
                other => panic!("expected configuration, got {other:?}"),
            }
        }
        panic!("configuration did not arrive");
    }

    fn parameters(input: &[u8]) -> Result<Option<(u8, u8)>> {
        legacy_aac_lc_parameters(&configuration(input), 0)
            .map(|parameters| parameters.map(|p| (p.frequency_index, p.channel_configuration)))
    }

    #[test]
    fn indexed_legacy_aac_tracks_supply_adts_parameters() {
        for input in [
            LC,
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/aac-mpeg2-lc.mkv"),
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/aac-mpeg2-lc-laced.mkv"),
            include_bytes!("../../../playback/tests/fixtures/mkv-feed/aac-mpeg4-lc-timed.mkv"),
        ] {
            assert_eq!(parameters(input), Ok(Some((3, 2))));
        }
    }

    #[test]
    fn empty_private_data_and_ambiguous_channels_do_not_supply_parameters() {
        let empty = include_bytes!(
            "../../../playback/tests/fixtures/mkv-feed/aac-mpeg4-lc-empty-private.mkv"
        );
        let ambiguous = include_bytes!(
            "../../../playback/tests/fixtures/mkv-feed/aac-mpeg4-lc-ambiguous-layout.mkv"
        );
        assert_eq!(parameters(empty), Ok(None));
        assert_eq!(
            parameters(ambiguous),
            Err(Error::Unsupported("legacy Matroska AAC channel layout"))
        );
    }

    #[test]
    fn legacy_aac_uses_default_rate_and_channels_only_with_audio_settings() {
        let audio = LC
            .windows(4)
            .position(|bytes| bytes == [0xe1, 0x8d, 0xb5, 0x88])
            .unwrap();
        let mut input = LC.to_vec();
        // Replace both Audio children with a same-size Void.
        input[audio + 2..audio + 15].fill(0);
        input[audio + 2..audio + 4].copy_from_slice(&[0xec, 0x8b]);
        assert_eq!(parameters(&input), Ok(Some((11, 1))));
        input[audio] = 0xec;
        assert_eq!(
            parameters(&input),
            Err(Error::Unsupported(
                "legacy Matroska AAC needs Audio settings"
            ))
        );
    }
}
