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

mod layer1;
mod layer2;
mod layer3;
mod subband;
mod synthesis;

pub use triergon_codec::{AudioFrame, Budget, ChannelLayout, DecodeError, Error, Speaker};

use triergon_codec::{AudioDecoder, Charged, DownmixLevels, Reservation};
use triergon_framing::mpeg_audio::{MpegAudioHeader, MpegLayer};

use layer3::Layer3;
use synthesis::Synthesis;

/// Storage limits of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of storage that depends on the stream: the signal state, built with the
    /// first frame, and the output frame the decoder fills (at most 1152 samples of two
    /// channels). The default, 1 MiB, leaves a wide margin above both.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 1 << 20,
        }
    }
}

/// An MPEG-1/2 audio Layer I, II and III decoder. Each access unit is one frame,
/// header included.
#[derive(Debug)]
pub struct Decoder {
    budget: Budget,
    output_charge: Reservation,
    /// Built and charged with the first frame.
    state: Option<Charged<Box<State>>>,
}

/// Signal state, fixed in size.
#[derive(Debug)]
struct State {
    synthesis: [Synthesis; 2],
    layer3: Layer3,
    mono: ChannelLayout,
    stereo: ChannelLayout,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder with a pool of the default limit.
    pub fn new() -> Self {
        #[expect(
            clippy::expect_used,
            reason = "the default limit is far above a pool's own charge"
        )]
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`. Fails only for a
    /// limit below a pool's own charge.
    pub fn with_limits(limits: Limits) -> Result<Self, DecodeError> {
        Ok(Self::new_in(&Budget::new(limits.live_storage_bytes)?))
    }

    /// A decoder charging the caller's pool.
    pub fn new_in(budget: &Budget) -> Self {
        Decoder {
            budget: budget.clone(),
            output_charge: Reservation::empty(budget),
            state: None,
        }
    }

    /// Decode one frame into `out`. A frame cut short decodes as if the missing bytes
    /// were zero; bytes after the frame are ignored. A frame refused after its header
    /// leaves `out` empty.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let header = MpegAudioHeader::from_bytes(au).ok_or(DecodeError::Invalid("frame header"))?;
        let length = header
            .frame_bytes()
            .ok_or(DecodeError::Unsupported("free-format frame"))?;
        let decoded = self.decode_body(&header, &au[..length.min(au.len())], out);
        if decoded.is_err() {
            out.clear();
        }
        decoded
    }

    /// Drop the bit reservoir and the filter bank and overlap history after a
    /// discontinuity. Layer III scalefactors stay, as FFmpeg keeps them for a granule
    /// that reuses them (`scfsi`) after a flush.
    pub fn reset(&mut self) {
        if let Some(state) = &mut self.state {
            for filter in &mut state.synthesis {
                filter.reset();
            }
            state.layer3.reset();
        }
    }

    fn decode_body(
        &mut self,
        header: &MpegAudioHeader,
        frame: &[u8],
        out: &mut AudioFrame,
    ) -> Result<(), DecodeError> {
        let state: &mut State = match &mut self.state {
            Some(state) => state,
            slot => slot.insert(Charged::boxed(&self.budget, State::new())?),
        };
        let channels = usize::from(header.channels());
        let samples = header.samples_per_frame() as usize;
        out.try_resize(channels, samples, &mut self.output_charge)?;
        out.sample_rate = header.sample_rate;
        out.layout = if channels == 1 {
            state.mono
        } else {
            state.stereo
        };
        out.samples_per_channel = samples;
        let planes = &mut out.planes;
        match header.layer {
            MpegLayer::I => layer1::decode(header, frame, &mut state.synthesis, planes),
            MpegLayer::II => layer2::decode(header, frame, &mut state.synthesis, planes),
            MpegLayer::III => {
                state
                    .layer3
                    .decode(header, frame, &mut state.synthesis, planes)?;
            }
        }
        Ok(())
    }
}

impl AudioDecoder for Decoder {
    type Error = DecodeError;

    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeError> {
        Decoder::decode(self, au, out)?;
        Ok(None)
    }

    fn reset(&mut self) {
        Decoder::reset(self);
    }
}

impl State {
    fn new() -> State {
        State {
            synthesis: [Synthesis::default(), Synthesis::default()],
            layer3: Layer3::new(),
            mono: ChannelLayout::new([Speaker::FrontCenter]).unwrap_or_default(),
            stereo: ChannelLayout::new([Speaker::FrontLeft, Speaker::FrontRight])
                .unwrap_or_default(),
        }
    }
}
