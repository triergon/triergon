use crate::{
    fields,
    movie::{SampleDescription, TrackHeader},
    quicktime::{PcmEncoding, SoundDescriptionV0},
};
use triergon_core::{Error, Result};

/// A QuickTime sound entry with a checked PCM frame layout.
#[derive(Clone, Debug)]
pub struct PcmDescription {
    source: SampleDescription,
    encoding: PcmEncoding,
    channels: u32,
    frame_bytes: usize,
    sample_rate: f64,
    revision: u16,
    vendor: [u8; 4],
}

impl PcmDescription {
    pub(super) fn parse(header: &TrackHeader, entry: &SampleDescription) -> Result<Self> {
        let body = entry.raw().payload();
        let version = fields::u16_at(body, 8)?;
        let prefix = match version {
            0 | 1 => SoundDescriptionV0::parse_prefix(header, entry, version)?,
            _ => return Err(Error::Unsupported("QT sound description version1/2")),
        };
        let layout = prefix.pcm_layout()?;
        let channels = u32::from(layout.channels());
        let frame_bytes = layout.frame_bytes();
        if version == 1 {
            // QuickTime treats the extended sizes as unused when samplesPerPacket is zero.
            let frames = fields::u32_at(body, 28)?;
            let channel_bytes = (frame_bytes / channels as usize) as u32;
            let bytes_per_packet = fields::u32_at(body, 32)?;
            let bytes_per_frame = fields::u32_at(body, 36)?;
            let bytes_per_sample = fields::u32_at(body, 40)?;
            if frames != 0
                && (frames != 1
                    || bytes_per_packet != channel_bytes
                    || bytes_per_frame as usize != frame_bytes
                    || bytes_per_sample != channel_bytes)
            {
                return Err(Error::Unsupported("QT version-1 PCM packet layout"));
            }
        }
        Ok(Self {
            revision: prefix.revision(),
            vendor: prefix.vendor(),
            source: entry.clone(),
            encoding: layout.encoding(),
            channels,
            frame_bytes,
            sample_rate: f64::from(layout.sample_rate_16_16()) / 65_536.0,
        })
    }

    /// The sample entry as stored, including its sound description version and extensions.
    pub fn source(&self) -> &SampleDescription {
        &self.source
    }

    /// Encoding of a single channel sample.
    pub fn encoding(&self) -> PcmEncoding {
        self.encoding
    }

    /// Number of channels in one interleaved frame.
    pub fn channels(&self) -> u32 {
        self.channels
    }

    /// Bytes in one interleaved frame.
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Sound description's sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub(super) fn revision(&self) -> u16 {
        self.revision
    }

    pub(super) fn vendor(&self) -> [u8; 4] {
        self.vendor
    }
}
