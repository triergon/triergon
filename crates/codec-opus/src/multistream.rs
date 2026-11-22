//! Channel mapping (RFC 7845 section 5.1.1 and RFC 8486): which decoded stream channel each output
//! channel takes, the output layout, and the header's output gain.

use crate::DecodeError;
use triergon_codec::layout::{native_order, vorbis_order};
use triergon_codec::{Budget, BudgetVec, ChannelLayout};
use triergon_framing::{OpusHead, OpusProjection};

/// No stream feeds this channel: it is silent.
const SILENT: u8 = 255;

/// The channel mapping of one decoder.
#[derive(Debug)]
pub(crate) struct Mapping {
    streams: usize,
    coupled: usize,
    /// Direct sources or column-major projection coefficients.
    routing: Routing,
    layout: ChannelLayout,
    /// Linear output gain, `None` at 0 dB.
    gain: Option<f32>,
}

#[derive(Debug)]
enum Routing {
    Direct([u8; ChannelLayout::CAPACITY]),
    Projection(BudgetVec<i16>),
}

fn ambisonic(channels: usize) -> Result<ChannelLayout, DecodeError> {
    let side = channels.isqrt();
    let base = side * side;
    let stereo = channels == base + 2;
    let order = side.saturating_sub(1) as u8;
    if channels != base && !stereo {
        return Err(DecodeError::Invalid("Opus Ambisonic channel count"));
    }
    ChannelLayout::ambisonic(order, stereo)
        .ok_or(DecodeError::Invalid("Opus Ambisonic channel count"))
}

fn gain(value: i16) -> Option<f32> {
    (value != 0).then(|| 10f64.powf(f64::from(value) / (20.0 * 256.0)) as f32)
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
            2 => (ambisonic(channels)?, core::array::from_fn(|i| i as u8)),
            255 => unordered(channels),
            _ => return Err(DecodeError::Unsupported("Opus channel mapping family")),
        };
        let mut source = [SILENT; ChannelLayout::CAPACITY];
        for (coded, &stream_channel) in head.mapping().iter().enumerate() {
            source[usize::from(plane[coded])] = stream_channel;
        }
        let gain = gain(head.output_gain);
        Ok(Mapping {
            streams: usize::from(head.streams),
            coupled: usize::from(head.coupled_streams),
            routing: Routing::Direct(source),
            layout,
            gain,
        })
    }

    pub(crate) fn projection(
        head: &OpusProjection<'_>,
        budget: &Budget,
    ) -> Result<Self, DecodeError> {
        let channels = usize::from(head.channels);
        if channels > ChannelLayout::CAPACITY {
            return Err(DecodeError::Unsupported("more than 93 output channels"));
        }
        let layout = ambisonic(channels)?;
        let mut coefficients = BudgetVec::new(budget);
        coefficients.try_reserve_exact(head.coefficients().len())?;
        for coefficient in head.coefficients() {
            coefficients
                .try_push(coefficient)
                .map_err(|error| error.error)?;
        }
        Ok(Self {
            streams: usize::from(head.streams),
            coupled: usize::from(head.coupled_streams),
            routing: Routing::Projection(coefficients),
            layout,
            gain: gain(head.output_gain),
        })
    }

    pub(crate) fn is_projection(&self) -> bool {
        matches!(self.routing, Routing::Projection(_))
    }

    pub(crate) fn prepare(&self, samples: usize, planes: &mut [Vec<f32>]) {
        if self.is_projection() {
            for plane in planes {
                plane[..samples].fill(0.0);
            }
        }
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

    /// Route or mix the decoded channels of `stream` into the output planes.
    pub(crate) fn distribute(
        &self,
        stream: usize,
        pcm: &[BudgetVec<f32>; 2],
        samples: usize,
        planes: &mut [Vec<f32>],
    ) {
        match &self.routing {
            Routing::Direct(source) => {
                for (plane, &index) in planes.iter_mut().zip(source) {
                    if let Some((_, channel)) = self.locate(index).filter(|&(s, _)| s == stream) {
                        plane[..samples].copy_from_slice(&pcm[channel][..samples]);
                    }
                }
            }
            Routing::Projection(matrix) => {
                let first = if stream < self.coupled {
                    2 * stream
                } else {
                    stream + self.coupled
                };
                let channels = self.channels();
                for (channel, input) in pcm.iter().enumerate().take(self.stream_channels(stream)) {
                    let start = (first + channel) * channels;
                    let column = &matrix[start..start + channels];
                    for (plane, &coefficient) in planes.iter_mut().zip(column) {
                        if coefficient == 0 {
                            continue;
                        }
                        let coefficient = f32::from(coefficient) * (1.0 / 32768.0);
                        for (output, &sample) in plane[..samples].iter_mut().zip(&input[..samples])
                        {
                            *output += coefficient * sample;
                        }
                    }
                }
            }
        }
    }

    /// The stream and channel within it that each output plane takes, `None` for a
    /// silent plane, in plane order.
    pub(crate) fn sources(&self) -> impl Iterator<Item = Option<(usize, usize)>> + '_ {
        let source = match &self.routing {
            Routing::Direct(source) => &source[..self.channels()],
            Routing::Projection(_) => &[],
        };
        source.iter().map(|&index| self.locate(index))
    }

    /// Apply the output gain to the planes the streams fed, and silence the others.
    pub(crate) fn finish(&self, samples: usize, planes: &mut [Vec<f32>]) {
        match &self.routing {
            Routing::Direct(source) => {
                for (plane, &index) in planes.iter_mut().zip(source) {
                    let plane = &mut plane[..samples];
                    match (self.locate(index), self.gain) {
                        (None, _) => plane.fill(0.0),
                        (Some(_), Some(gain)) => {
                            for value in plane {
                                *value *= gain;
                            }
                        }
                        (Some(_), None) => {}
                    }
                }
            }
            Routing::Projection(_) => {
                if let Some(gain) = self.gain {
                    for plane in planes {
                        for value in &mut plane[..samples] {
                            *value *= gain;
                        }
                    }
                }
            }
        }
    }
}
