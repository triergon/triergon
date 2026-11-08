#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

mod celt;
mod multistream;
mod packet;
mod range;
mod silk;
mod stream;

use multistream::Mapping;
use packet::Packet;
use stream::{StreamDecoder, Workspace};
use triergon_codec::{
    AudioDecoder, AudioFrame, Budget, ChannelLayout, DecodeFailure, DownmixLevels, Reservation,
    StorageError,
};
use triergon_core::BudgetVec;
use triergon_framing::{OPUS_RATE, OpusHead};

/// Bounds of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Heap bytes the decoder may hold for its streams and the output frame.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    /// 64 MiB: 255 streams of state (about 30 KiB each) and the largest output frame
    /// (255 channels of 120 ms) fit with a wide margin.
    fn default() -> Self {
        Limits {
            live_storage_bytes: 64 << 20,
        }
    }
}

/// Why a decoder refused a configuration or an access unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A feature this decoder does not implement.
    Unsupported(&'static str),
    /// Malformed data.
    Invalid(&'static str),
    /// The storage budget refused an allocation.
    Budget(StorageError),
}

impl From<StorageError> for DecodeError {
    fn from(error: StorageError) -> Self {
        DecodeError::Budget(error)
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Unsupported(what) => write!(f, "unsupported: {what}"),
            DecodeError::Invalid(what) => write!(f, "invalid data: {what}"),
            DecodeError::Budget(error) => write!(f, "storage budget: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// An Opus decoder for one logical stream: a single Opus stream or a multistream
/// (RFC 7845 section 5.1.1), decoding to 48 kHz.
///
/// Output planes follow FFmpeg's channel order. The header's output gain is applied;
/// its pre-skip is reported as the stream's start [`delay`](AudioDecoder::delay) for the
/// consumer to discard, as containers may state it themselves.
#[derive(Debug)]
pub struct Decoder {
    mapping: Mapping,
    pre_skip: u16,
    streams: BudgetVec<StreamDecoder>,
    work: Box<Workspace>,
    output_charge: Reservation,
}

impl Decoder {
    /// A decoder for an `OpusHead` identification header (RFC 7845 section 5.1), the
    /// canonical configuration of every container.
    pub fn from_configuration(
        configuration: &[u8],
        limits: Limits,
    ) -> Result<Decoder, DecodeError> {
        let budget = Budget::new(limits.live_storage_bytes)?;
        Decoder::from_configuration_in(configuration, &budget)
    }

    /// As [`Decoder::from_configuration`], charging the caller's pool.
    pub fn from_configuration_in(
        configuration: &[u8],
        budget: &Budget,
    ) -> Result<Decoder, DecodeError> {
        let head = OpusHead::parse(configuration)
            .map_err(|_| DecodeError::Invalid("Opus identification header"))?;
        let mapping = Mapping::new(&head)?;
        let mut streams = BudgetVec::new(budget);
        streams.try_reserve_exact(mapping.streams())?;
        for stream in 0..mapping.streams() {
            streams
                .try_push(StreamDecoder::new(mapping.stream_channels(stream)))
                .map_err(|error| error.error)?;
        }
        Ok(Decoder {
            mapping,
            pre_skip: head.pre_skip,
            streams,
            work: Box::new(Workspace::new()),
            output_charge: Reservation::empty(budget),
        })
    }

    /// The output channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.mapping.layout()
    }

    /// Range coder state after the last packet, XORed over the streams: the check value
    /// RFC 6716 test vectors record per packet.
    #[doc(hidden)]
    pub fn final_range(&self) -> u32 {
        self.streams
            .iter()
            .fold(0, |range, stream| range ^ stream.final_range())
    }

    /// Emit the samples still held back at the end of the stream: the SILK resampler
    /// delays the first narrowband frame after a start by up to 24 samples and releases
    /// them when SILK stops, changes rate, or here. `out` is replaced (possibly empty).
    pub fn flush(&mut self, out: &mut AudioFrame) -> Result<(), DecodeError> {
        out.samples_per_channel = 0;
        let mut samples = 0;
        for index in 0..self.streams.len() {
            let produced = self.streams[index].flush(&mut self.work);
            self.collect(index, produced, &mut samples, out)?;
        }
        self.finish(samples, out);
        Ok(())
    }

    fn decode_packet(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        out.samples_per_channel = 0;
        let mut rest = au;
        let mut samples = 0;
        let last = self.streams.len() - 1;
        for index in 0..self.streams.len() {
            let packet = Packet::parse(rest, index != last)
                .map_err(|e| DecodeError::Invalid(e.message()))?;
            rest = &rest[packet.len..];
            let produced = self.streams[index]
                .decode(&packet, &mut self.work)
                .map_err(|e| DecodeError::Invalid(e.message()))?;
            self.collect(index, produced, &mut samples, out)?;
        }
        self.finish(samples, out);
        Ok(())
    }

    /// Move stream `index`'s `produced` samples to their planes; every stream must
    /// produce as many as the first.
    fn collect(
        &mut self,
        index: usize,
        produced: usize,
        samples: &mut usize,
        out: &mut AudioFrame,
    ) -> Result<(), DecodeError> {
        if index == 0 {
            *samples = produced;
            out.try_resize(self.mapping.channels(), produced, &mut self.output_charge)?;
        } else if produced != *samples {
            return Err(DecodeError::Invalid("streams of unequal duration"));
        }
        self.mapping
            .distribute(index, &self.work.pcm, produced, &mut out.planes);
        Ok(())
    }

    fn finish(&self, samples: usize, out: &mut AudioFrame) {
        self.mapping.finish(samples, &mut out.planes);
        out.sample_rate = OPUS_RATE;
        out.layout = self.mapping.layout();
        out.samples_per_channel = samples;
    }
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        self.decode_packet(au, out)
            .map(|()| None)
            .map_err(DecodeFailure::from_error)
    }

    fn delay(&self) -> usize {
        usize::from(self.pre_skip)
    }

    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), DecodeFailure> {
        Decoder::flush(self, out).map_err(DecodeFailure::from_error)
    }

    fn reset(&mut self) {
        for stream in self.streams.iter_mut() {
            stream.reset();
        }
    }
}

#[cfg(test)]
mod tests;
