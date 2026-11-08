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

mod layer1;
mod layer2;
mod layer3;
mod subband;
mod synthesis;

use triergon_codec::{
    AudioDecoder, AudioFrame, Budget, ChannelLayout, Charged, DecodeFailure, DownmixLevels,
    Reservation, Speaker, StorageError,
};
use triergon_framing::{MpegAudioHeader, MpegLayer};

use layer3::Layer3;
use synthesis::Synthesis;

/// Bounds of one decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Heap bytes the decoder may hold: its signal state, built with the first frame,
    /// and the output frame it fills (at most 1152 samples of two channels). The default
    /// leaves a wide margin above both.
    pub live_storage_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            live_storage_bytes: 1 << 20,
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
    /// A decoder with the default limits.
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // The default limit is far above a pool's own charge.
        Self::with_limits(Limits::default()).expect("default storage budget")
    }

    /// A decoder with its own pool of `limits.live_storage_bytes`.
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
    pub fn decode_frame(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        let header = frame_header(au).ok_or(DecodeError::Invalid("frame header"))?;
        let length = header
            .frame_bytes()
            .ok_or(DecodeError::Unsupported("free-format frame"))?;
        let decoded = self.decode_body(&header, &au[..length.min(au.len())], out);
        if decoded.is_err() {
            out.clear();
        }
        decoded
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

/// The header of the frame at the start of `bytes`.
pub fn frame_header(bytes: &[u8]) -> Option<MpegAudioHeader> {
    MpegAudioHeader::from_bytes(bytes)
}

impl AudioDecoder for Decoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        self.decode_frame(au, out)
            .map_err(DecodeFailure::from_error)?;
        Ok(None)
    }

    fn reset(&mut self) {
        if let Some(state) = &mut self.state {
            for filter in &mut state.synthesis {
                filter.reset();
            }
            state.layer3.reset();
        }
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
