//! Decoding the packets of one container audio track.
use triergon_codec::{AudioDecoder, AudioFrame, DecodeFailure, DownmixLevels};
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_core::{Rounding, TimeBase};

use crate::{AudioCodec, audio_codec, audio_configuration, audio_decoder};

/// The decoder of one container audio track. It chooses the decoder from the track's
/// codec and configuration, and drops the decoded samples a packet's container marks
/// for discarding: its [`Trim`](triergon_core::container::Trim), any samples before
/// time zero (Matroska CodecDelay, Opus pre-skip in Ogg), and at least the decoder's own
/// start delay ([`AudioDecoder::delay`]) at the start of the stream.
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
    /// The decoder for `track`; refused when this crate does not decode its codec or its
    /// configuration is invalid.
    pub fn new(track: &dyn TrackInfo) -> Result<TrackDecoder, DecodeFailure> {
        let codec = audio_codec(track.codec())
            .ok_or_else(|| DecodeFailure(format!("unsupported codec {:?}", track.codec())))?;
        let mut configuration = Vec::new();
        audio_configuration(codec, track.configuration(), &mut configuration)
            .map_err(DecodeFailure::from_error)?;
        let decoder = audio_decoder(codec, &configuration)?;
        Ok(TrackDecoder {
            codec,
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
        let len = out.samples_per_channel;
        if len == 0 {
            return Ok(levels);
        }
        let samples = |ticks: i64| to_samples(ticks, packet.time_base(), out.sample_rate);
        let trim = packet.trim();
        let before_zero = packet
            .pts()
            .filter(|&pts| pts < 0)
            .map_or(0, |pts| samples(-pts));
        let start = trim
            .start
            .map_or(0, samples)
            .max(before_zero)
            .max(self.delay)
            .min(len);
        self.delay -= start.min(self.delay);
        let end = trim.end.map_or(0, samples).min(len - start);
        if start > 0 || end > 0 {
            for plane in &mut out.planes {
                plane.copy_within(start..len - end, 0);
                plane.truncate(len - start - end);
            }
            out.samples_per_channel = len - start - end;
        }
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
}

/// Samples at `rate` in `ticks` of `base`, rounded to the nearest; 0 when it does not fit.
fn to_samples(ticks: i64, base: TimeBase, rate: u32) -> usize {
    TimeBase::new(1, u64::from(rate.max(1)))
        .and_then(|samples| base.rescale(ticks.max(0), samples, Rounding::NearestAway))
        .ok()
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}
