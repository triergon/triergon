//! Decoding the packets of one container audio track.
use triergon_codec::{AudioDecoder, AudioFrame, Budget, DecodeFailure, DownmixLevels};
use triergon_core::BudgetVec;
use triergon_core::container::{PacketInfo, TrackInfo};

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
