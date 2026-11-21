#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod celt;
mod multistream;
mod packet;
mod range;
mod silk;
mod stream;

use celt::Emphasis;
use multistream::Mapping;
use packet::Packet;
use stream::{Staging, StreamDecoder, Workspace};
pub use triergon_codec::DecodeError;
use triergon_codec::{
    AudioDecoder, AudioFrame, Budget, BudgetVec, ChannelLayout, Charged, DownmixLevels, Reservation,
};
use triergon_framing::opus::{self, OpusHead};

/// Bounds of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Heap bytes the decoder may hold for its streams and the output frame.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    /// 64 MiB: the state of 255 streams and the largest output frame (93 channels of
    /// 120 ms) fit with a wide margin.
    fn default() -> Self {
        Limits {
            live_storage_bytes: 64 << 20,
        }
    }
}

/// An Opus decoder for one logical stream: a single Opus stream or a multistream
/// (RFC 7845 section 5.1.1), decoding to 48 kHz.
///
/// Output planes follow FFmpeg's channel order. The header's output gain is applied;
/// its pre-skip is reported as the stream's start [`delay`](AudioDecoder::delay) for the
/// consumer to discard, as containers may state it themselves.
///
/// SILK output may lag its packet: after a start, the SILK resampler holds back up to
/// 24 samples of the first narrowband frame and releases them ahead of the next frame
/// once SILK stops or changes its internal rate. That packet's output is longer than
/// its duration by as many samples, and the first one's shorter. At the end of the
/// stream the samples still held come out of [`Decoder::flush`] (or
/// [`AudioDecoder::flush`]); a consumer that skips it loses them. CELT-only streams
/// hold nothing back.
#[derive(Debug)]
pub struct Decoder {
    mapping: Mapping,
    pre_skip: u16,
    streams: BudgetVec<StreamDecoder>,
    /// What checking a packet reads of every stream, for decoding it.
    staging: Staging,
    work: Charged<Box<Workspace>>,
    output_charge: Reservation,
}

impl Decoder {
    /// A decoder for an `OpusHead` identification header (RFC 7845 section 5.1), the
    /// canonical configuration of every container, with a pool of the default limit.
    pub fn new(configuration: &[u8]) -> Result<Decoder, DecodeError> {
        Decoder::with_limits(configuration, Limits::default())
    }

    /// As [`Decoder::new`], with its own pool of `limits.live_storage_bytes`.
    pub fn with_limits(configuration: &[u8], limits: Limits) -> Result<Decoder, DecodeError> {
        Decoder::new_in(configuration, &Budget::new(limits.live_storage_bytes)?)
    }

    /// As [`Decoder::new`], charging the caller's pool.
    pub fn new_in(configuration: &[u8], budget: &Budget) -> Result<Decoder, DecodeError> {
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
            staging: Staging::new(streams.len(), budget)?,
            streams,
            work: Charged::boxed(budget, Workspace::new(budget)?)?,
            output_charge: Reservation::empty(budget),
        })
    }

    /// The output channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.mapping.layout()
    }

    /// The range decoder's final `rng` after the last frame of the last packet, XORed
    /// with that of its redundant frame and over the streams. A stream whose last frame
    /// has at most one byte contributes 0, as one does after [`AudioDecoder::reset`].
    /// The RFC 6716 test vectors record this value per packet.
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
        let samples = self.streams.first().map_or(0, StreamDecoder::pending);
        if self
            .streams
            .iter()
            .any(|stream| stream.pending() != samples)
        {
            return Err(DecodeError::Invalid("streams of unequal duration"));
        }
        out.try_resize(self.mapping.channels(), samples, &mut self.output_charge)?;
        for (index, stream) in self.streams.iter_mut().enumerate() {
            let produced = stream.flush(&mut self.work);
            debug_assert_eq!(produced, samples);
            self.mapping
                .distribute(index, &self.work.pcm, samples, &mut out.planes);
        }
        self.finish(samples, out);
        Ok(())
    }

    /// Decode one (multistream) packet. Everything that can refuse it, from the framing
    /// of each sub-packet to the output frame's charge, comes before the first stream
    /// decodes, so a refused packet leaves every stream as it was.
    fn decode_packet(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        out.samples_per_channel = 0;
        let (samples, emphasis) = self.check(au)?;
        out.try_resize(self.mapping.channels(), samples, &mut self.output_charge)?;
        // `check` parsed every sub-packet.
        let packets = sub_packets(au, self.streams.len()).map_while(Result::ok);
        for (index, (stream, packet)) in self.streams.iter_mut().zip(packets).enumerate() {
            let staged = self.staging.stream(index);
            let produced = stream.decode(&packet, staged, &mut self.work, emphasis);
            debug_assert_eq!(produced, samples);
            self.mapping
                .distribute(index, &self.work.pcm, samples, &mut out.planes);
        }
        if emphasis == Emphasis::Defer {
            self.deemphasise(samples, &mut out.planes);
        }
        self.finish(samples, out);
        Ok(())
    }

    /// De-emphasise the first `samples` of every plane a stream channel feeds, with and
    /// into that channel's filter memory. A channel that feeds several planes runs from
    /// the same memory in each and leaves the same memory. Not inlined: its per-plane
    /// arrays would otherwise enlarge the stack frame of every packet's decoding.
    #[inline(never)]
    fn deemphasise(&mut self, samples: usize, planes: &mut [Vec<f32>]) {
        const CAPACITY: usize = ChannelLayout::CAPACITY;
        let mut memory = [0f32; CAPACITY];
        let mut lanes: [&mut [f32]; CAPACITY] = core::array::from_fn(|_| <&mut [f32]>::default());
        let mut fed = 0;
        for (plane, source) in planes.iter_mut().zip(self.mapping.sources()) {
            if let Some((stream, channel)) = source {
                memory[fed] = self.streams[stream].deemphasis_memory()[channel];
                lanes[fed] = &mut plane[..samples];
                fed += 1;
            }
        }
        celt::deemphasise_channels(&mut memory[..fed], &mut lanes[..fed]);
        for (&m, source) in memory.iter().zip(self.mapping.sources().flatten()) {
            let (stream, channel) = source;
            self.streams[stream].deemphasis_memory()[channel] = m;
        }
    }

    /// Check every sub-packet of `au` against its stream, staging what decoding them
    /// continues from, and return the samples per channel they decode to, which must
    /// agree, and where their de-emphasis runs: for all channels together once the
    /// streams are decoded when every stream decodes CELT frames alone.
    fn check(&mut self, au: &[u8]) -> Result<(usize, Emphasis), DecodeError> {
        let mut samples = None;
        let mut emphasis = Emphasis::Defer;
        let packets = sub_packets(au, self.streams.len());
        for (index, (stream, packet)) in self.streams.iter().zip(packets).enumerate() {
            let packet = packet?;
            let len = stream.check(&packet, self.staging.stream_mut(index))?;
            if *samples.get_or_insert(len) != len {
                return Err(DecodeError::Invalid("streams of unequal duration"));
            }
            if !stream.can_defer_emphasis(&packet) {
                emphasis = Emphasis::Apply;
            }
        }
        Ok((samples.unwrap_or(0), emphasis))
    }

    fn finish(&self, samples: usize, out: &mut AudioFrame) {
        self.mapping.finish(samples, &mut out.planes);
        out.sample_rate = opus::SAMPLE_RATE;
        out.layout = self.mapping.layout();
        out.samples_per_channel = samples;
    }
}

/// The sub-packets of the `streams` streams of a packet: all but the last are
/// self-delimited (RFC 6716 Appendix B), and the last takes the rest.
fn sub_packets(au: &[u8], streams: usize) -> impl Iterator<Item = Result<Packet<'_>, DecodeError>> {
    let mut rest = au;
    (0..streams).map(move |index| {
        let packet = Packet::parse(rest, index + 1 < streams)
            .map_err(|error| DecodeError::Invalid(error.message()))?;
        rest = &rest[packet.len..];
        Ok(packet)
    })
}

impl AudioDecoder for Decoder {
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        self.decode_packet(au, out).map(|()| None)
    }

    fn delay(&self) -> usize {
        usize::from(self.pre_skip)
    }

    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), DecodeError> {
        Decoder::flush(self, out)
    }

    fn reset(&mut self) {
        for stream in self.streams.iter_mut() {
            stream.reset();
        }
    }
}

#[cfg(test)]
mod tests;
