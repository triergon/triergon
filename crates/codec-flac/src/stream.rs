//! Stream parameters from the configuration, and the format of each frame.
use crate::layout::{default_layout, masked_layout};
use crate::{DecodeError, FrameInfo};
use triergon_codec::ChannelLayout;
use triergon_framing::{
    FLAC_STREAMINFO, FLAC_VORBIS_COMMENT, FlacFrameHeader, FlacStreamInfo, flac_metadata_blocks,
};

/// What the configuration fixes for every frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stream {
    info: Option<FlacStreamInfo>,
    /// STREAMINFO's layout, which a frame with as many channels uses.
    layout: ChannelLayout,
    /// The stream's sample size: STREAMINFO's, else that of the first frame.
    bits_per_sample: Option<u8>,
}

impl Stream {
    /// Parameters of a canonical configuration, or of none (empty bytes): a stream
    /// picked up at a frame header, whose frames then carry their own parameters.
    pub(crate) fn from_configuration(configuration: &[u8]) -> Result<Stream, DecodeError> {
        if configuration.is_empty() {
            return Ok(Stream {
                info: None,
                layout: ChannelLayout::EMPTY,
                bits_per_sample: None,
            });
        }
        let invalid = |_| DecodeError::Invalid("FLAC configuration");
        let mut blocks = flac_metadata_blocks(configuration).map_err(invalid)?;
        let first = blocks
            .next()
            .ok_or(DecodeError::Invalid("FLAC configuration"))?
            .map_err(invalid)?;
        if first.kind != FLAC_STREAMINFO {
            return Err(DecodeError::Invalid(
                "first metadata block is not STREAMINFO",
            ));
        }
        let info = FlacStreamInfo::parse(first.body).map_err(invalid)?;
        if info.max_block_size < 16 {
            // RFC 9639 section 8.2; FFmpeg refuses to open such a stream.
            return Err(DecodeError::Invalid(
                "STREAMINFO maximum block size below 16",
            ));
        }
        // Later blocks are optional: a damaged one ends the search for a channel mask.
        let layout = blocks
            .map_while(Result::ok)
            .find(|block| block.kind == FLAC_VORBIS_COMMENT)
            .and_then(|block| masked_layout(block.body, info.channels))
            .unwrap_or_else(|| default_layout(info.channels));
        Ok(Stream {
            info: Some(info),
            layout,
            bits_per_sample: Some(info.bits_per_sample),
        })
    }

    /// The format of the frame `header` starts, as FFmpeg resolves it: the header's rate
    /// and sample size where it codes them, else STREAMINFO's; a sample size other than
    /// the stream's refused; STREAMINFO's maximum block size enforced; the stream layout
    /// while the channel count matches STREAMINFO, the default layout otherwise.
    pub(crate) fn frame(&mut self, header: &FlacFrameHeader) -> Result<FrameInfo, DecodeError> {
        let info = self.info.as_ref();
        let sample_rate = header
            .sample_rate
            .or(info.map(|info| info.sample_rate))
            .filter(|&rate| rate > 0)
            .ok_or(DecodeError::Invalid("frame without a sample rate"))?;
        let bits_per_sample = header
            .bits_per_sample
            .or(self.bits_per_sample)
            .ok_or(DecodeError::Invalid("frame without a sample size"))?;
        match self.bits_per_sample {
            Some(stream) if stream != bits_per_sample => {
                return Err(DecodeError::Unsupported(
                    "sample size change within a stream",
                ));
            }
            _ => self.bits_per_sample = Some(bits_per_sample),
        }
        if let Some(info) = info
            && header.block_size > u32::from(info.max_block_size)
        {
            return Err(DecodeError::Invalid(
                "block size above the STREAMINFO maximum",
            ));
        }
        let channels = header.channels.channels();
        let layout = match info {
            Some(info) if info.channels == channels => self.layout,
            _ => default_layout(channels),
        };
        Ok(FrameInfo {
            sample_rate,
            bits_per_sample,
            layout,
            samples_per_channel: header.block_size as usize,
        })
    }
}
