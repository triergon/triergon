//! DTS container signalling for the explicitly selected core playback path.
use crate::{BoxChildren, BoxView, EntryLayout, Limits, SampleDescription, fields};
use triergon_core::{Error, Result, TimeBase};

/// A sample entry eligible for DTS core playback. Packet inspection is still
/// required to establish the actual core, its size and its decoder parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DtsCoreDescription {
    sample_entry_rate_hz: u32,
    access_unit_clock: Option<(u32, u32)>,
}

impl DtsCoreDescription {
    /// Integral sample rate declared by the audio sample entry.
    pub fn sample_entry_rate_hz(self) -> u32 {
        self.sample_entry_rate_hz
    }

    /// Media ticks per sample-entry tick and, when signalled, per access unit.
    pub(crate) fn media_clock(self, base: TimeBase) -> Result<(u32, Option<u32>)> {
        let rate = u64::from(self.sample_entry_rate_hz);
        let scale = base.denominator();
        if base.numerator() != 1 || !scale.is_multiple_of(rate) {
            return Err(Error::Unsupported("DTS media timescale"));
        }
        let frame = self
            .access_unit_clock
            .map(|(frequency, samples)| {
                let ticks = scale
                    .checked_mul(u64::from(samples))
                    .ok_or(Error::Overflow)?;
                let frequency = u64::from(frequency);
                if ticks % frequency != 0 {
                    return Err(Error::Unsupported("DTS fractional media frame duration"));
                }
                u32::try_from(ticks / frequency).map_err(|_| Error::Overflow)
            })
            .transpose()?;
        Ok((
            u32::try_from(scale / rate).map_err(|_| Error::Overflow)?,
            frame,
        ))
    }
}

pub(crate) fn dts_entry_children(entry: &SampleDescription, limits: Limits) -> Result<BoxChildren> {
    if entry.format == *b"dtsc" && fields::u16_at(entry.raw().payload(), 8)? == 1 {
        crate::SoundV1Source::parse(entry)?;
        entry.raw().children(44, limits)
    } else {
        entry.extensions(EntryLayout::AudioV0, limits)
    }
}

fn audio_entry_rate(entry: &SampleDescription) -> Result<u32> {
    let b = entry.raw().payload();
    let sample_rate = fields::u32_at(b, 24)?;
    if entry.data_reference_index != 1
        || fields::u16_at(b, 6)? != 1
        || fields::u16_at(b, 16)? == 0
        || fields::u16_at(b, 18)? != 16
        || sample_rate >> 16 == 0
        || sample_rate & 0xffff != 0
    {
        return Err(Error::Unsupported("DTS audio sample entry"));
    }
    Ok(sample_rate >> 16)
}

pub(crate) fn check_dts_sample_entry(
    entry: &SampleDescription,
    config: Option<&BoxView>,
) -> Result<DtsCoreDescription> {
    let b = entry.raw().payload();
    let sample_entry_rate_hz = audio_entry_rate(entry)?;
    let access_unit_clock = match fields::u16_at(b, 8)? {
        0 => {
            if fields::slice(b, 10, 6)? != [0; 6] || fields::slice(b, 20, 4)? != [0; 4] {
                return Err(Error::Invalid("DTS AudioSampleEntry reserved fields"));
            }
            let config = config.ok_or(Error::Unsupported("missing DTS ddts box"))?;
            Some(check_ddts(
                entry.format,
                sample_entry_rate_hz,
                config.payload(),
            )?)
        }
        // FFmpeg 9.0.2's QuickTime DTS entry carries no ddts. Its v1
        // samples-per-packet word may be populated. stts supplies timing.
        1 if entry.format == *b"dtsc"
            && config.is_none()
            && fields::slice(b, 10, 6)? == [0; 6]
            && fields::slice(b, 20, 4)? == [0xff, 0xfe, 0, 0]
            && fields::slice(b, 32, 8)? == [0; 8]
            && fields::u32_at(b, 40)? == 2 =>
        {
            None
        }
        _ => return Err(Error::Unsupported("DTS QuickTime sample entry profile")),
    };
    Ok(DtsCoreDescription {
        sample_entry_rate_hz,
        access_unit_clock,
    })
}

// ETSI TS 102 114 V1.6.1, E.2.2.2 and E.2.2.3. The selected subset
// uses the fixed record and permits private sibling boxes under the entry.
fn check_ddts(format: [u8; 4], entry_rate: u32, b: &[u8]) -> Result<(u32, u32)> {
    let fixed = fields::slice(b, 0, 20)?;
    if b.len() != fixed.len() || fixed[19] != 0 {
        return Err(Error::Unsupported(
            "DTS ddts extensions, assets or reserved bits",
        ));
    }
    let frequency = fields::u32_at(fixed, 0)?;
    let base_rate = match frequency {
        24_000 | 48_000 | 96_000 | 192_000 => 48_000,
        22_050 | 44_100 | 88_200 | 176_400 => 44_100,
        16_000 | 32_000 | 64_000 | 128_000 => 32_000,
        _ => return Err(Error::Unsupported("DTS ddts sampling frequency")),
    };
    let construction = (fixed[13] >> 1) & 31;
    let core_layout = fixed[14] >> 2;
    let core_size = (fields::u32_at(fixed, 13)? >> 4) & 0x3fff;
    let has_core = match &format {
        b"dtsc" => matches!(construction, 1 | 2 | 4),
        b"dtsh" => matches!(construction, 3 | 5..=16),
        // Table E-2 names the core-plus-XLL rows dtsh. Accept the dtsl
        // label only with the same explicit core-plus-XLL construction.
        b"dtsl" => matches!(construction, 14..=16),
        _ => false,
    };
    if !has_core || core_size == 0 {
        return Err(Error::Unsupported(
            "DTS ddts has no selected core substream",
        ));
    }
    if entry_rate != base_rate
        || !matches!(fixed[12], 16 | 24)
        || !matches!(core_layout, 0 | 2 | 4..=9 | 31)
    {
        return Err(Error::Unsupported("DTS ddts audio parameters"));
    }
    let channel_layout = fields::u16_at(fixed, 17)?;
    if channel_layout == 0 && !matches!(fixed[16] & 7, 0 | 2 | 3) {
        return Err(Error::Unsupported("DTS ddts representation type"));
    }
    Ok((frequency, 512 << (fixed[13] >> 6)))
}

/// DTS object types from the MP4 Registration Authority's Object Types table.
/// Core or core plus extension can be verified by the packet consumer.
pub(crate) fn dts_object_type(object_type: u8) -> Result<bool> {
    match object_type {
        0xa9 | 0xaa => Ok(true),
        0xab | 0xac | 0xb2 | 0xb3 => {
            Err(Error::Unsupported("DTS object type without selected core"))
        }
        _ => Ok(false),
    }
}

pub(crate) fn check_mp4a_dts(entry: &SampleDescription) -> Result<DtsCoreDescription> {
    if fields::u16_at(entry.raw().payload(), 8)? != 0 {
        return Err(Error::Unsupported("DTS mp4a requires AudioSampleEntry v0"));
    }
    Ok(DtsCoreDescription {
        sample_entry_rate_hz: audio_entry_rate(entry)?,
        access_unit_clock: None,
    })
}
