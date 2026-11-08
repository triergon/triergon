//! Channel mapping (RFC 7845 section 5.1.1): which decoded stream channel each output
//! channel takes, the output layout, and the header's output gain.

use crate::DecodeError;
use triergon_codec::layout::{native_order, vorbis_order};
use triergon_codec::{BudgetVec, ChannelLayout};
use triergon_framing::OpusHead;

/// No stream feeds this channel: it is silent.
const SILENT: u8 = 255;

/// The channel mapping of one decoder.
#[derive(Clone, Debug)]
pub(crate) struct Mapping {
    streams: usize,
    coupled: usize,
    /// Per output plane, the stream channel it takes (`SILENT` for none), in plane order.
    source: [u8; ChannelLayout::CAPACITY],
    layout: ChannelLayout,
    /// Linear output gain, `None` at 0 dB.
    gain: Option<f32>,
}

/// Channels in coded order without speaker positions (FFmpeg's unspecified order),
/// `channels <= ChannelLayout::CAPACITY`.
fn unordered(channels: usize) -> (ChannelLayout, [u8; ChannelLayout::CAPACITY]) {
    let layout = ChannelLayout::unspecified(channels).unwrap_or_default();
    (layout, core::array::from_fn(|i| i as u8))
}

impl Mapping {
    pub(crate) fn new(head: &OpusHead) -> Result<Mapping, DecodeError> {
        let channels = usize::from(head.channels);
        // The most channels an output layout holds.
        if channels > ChannelLayout::CAPACITY {
            return Err(DecodeError::Unsupported("more than 93 output channels"));
        }
        // Each coded channel's output plane. Families 0 and 1 use the Vorbis order (one
        // and two channels are the same in both), which FFmpeg reports in native order.
        let (layout, plane) = match head.mapping_family {
            0 | 1 => vorbis_order(channels)
                .and_then(native_order)
                .ok_or(DecodeError::Invalid(
                    "Vorbis channel mapping beyond 8 channels",
                ))?,
            255 => unordered(channels),
            _ => return Err(DecodeError::Unsupported("Opus channel mapping family")),
        };
        let mut source = [SILENT; ChannelLayout::CAPACITY];
        for (coded, &stream_channel) in head.mapping().iter().enumerate() {
            source[usize::from(plane[coded])] = stream_channel;
        }
        let gain = (head.output_gain != 0)
            .then(|| 10f64.powf(f64::from(head.output_gain) / (20.0 * 256.0)) as f32);
        Ok(Mapping {
            streams: usize::from(head.streams),
            coupled: usize::from(head.coupled_streams),
            source,
            layout,
            gain,
        })
    }

    pub(crate) fn streams(&self) -> usize {
        self.streams
    }

    pub(crate) fn channels(&self) -> usize {
        self.layout.channels()
    }

    pub(crate) fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Channels of stream `stream`: the first streams are coupled pairs.
    pub(crate) fn stream_channels(&self, stream: usize) -> usize {
        if stream < self.coupled { 2 } else { 1 }
    }

    /// The stream and channel within it that stream channel index `index` names.
    fn locate(&self, index: u8) -> Option<(usize, usize)> {
        let index = usize::from(index);
        if index == usize::from(SILENT) {
            None
        } else if index < 2 * self.coupled {
            Some((index / 2, index % 2))
        } else {
            Some((index - self.coupled, 0))
        }
    }

    /// Copy the decoded channels of `stream` (`pcm[channel][..samples]`) to the planes
    /// that take them.
    pub(crate) fn distribute(
        &self,
        stream: usize,
        pcm: &[BudgetVec<f32>; 2],
        samples: usize,
        planes: &mut [Vec<f32>],
    ) {
        for (plane, &index) in planes.iter_mut().zip(&self.source) {
            if let Some((_, channel)) = self.locate(index).filter(|&(s, _)| s == stream) {
                let out = &mut plane[..samples];
                out.copy_from_slice(&pcm[channel][..samples]);
                if let Some(gain) = self.gain {
                    for v in out {
                        *v *= gain;
                    }
                }
            }
        }
    }

    /// Silence the planes no stream feeds.
    pub(crate) fn finish(&self, samples: usize, planes: &mut [Vec<f32>]) {
        for (plane, &index) in planes.iter_mut().zip(&self.source) {
            if self.locate(index).is_none() {
                plane[..samples].fill(0.0);
            }
        }
    }
}
