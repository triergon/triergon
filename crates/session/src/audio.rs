//! Audio lane: compressed frames in, clock-paced stereo s16 PCM out.
//!
//! Each access unit is held until the sink clock is within `lead` of its PTS;
//! `pull` then decodes exactly those units and returns their stereo samples for
//! the caller's device writer. A paused clock
//! therefore yields nothing, and flush/stop discard everything queued.
use crate::Discard;
use crate::SessionState;
use crate::lane::Phase;
use crate::mix::Downmix;
use crate::queue::{FeedError, QueueLimits, UnitQueue};
use triergon_codec::{AudioFrame, DownmixLevels, DynAudioDecoder};

/// One block of output: interleaved stereo s16 at one sample rate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcmBlock {
    /// Samples per second per channel; meaningful while `samples` is not empty.
    pub sample_rate: u32,
    /// The caller PTS of the first access unit in the block; `None` when that unit had
    /// none, or when the block holds only samples the decoder released at end of stream.
    pub pts: Option<i64>,
    /// Left and right samples in turn.
    pub samples: Vec<i16>,
}

/// The usual audio lead for [`AudioSession::pull`]: 120 ms.
pub const DEFAULT_LEAD_NS: i64 = 120_000_000;

/// The audio lane of a playback session: a bounded queue of compressed access units, a
/// decoder, and a stereo downmix paced by the caller's clock.
pub struct AudioSession {
    decoder: Option<Box<DynAudioDecoder>>,
    /// Samples of the decoder's start delay not discarded yet.
    delay: usize,
    queue: UnitQueue,
    frame: AudioFrame,
    output: Output,
    generation: u64,
    phase: Phase,
}

impl core::fmt::Debug for AudioSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioSession")
            .field("state", &self.state())
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl AudioSession {
    /// An idle lane whose queue holds at most `limits`.
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            decoder: None,
            delay: 0,
            queue: UnitQueue::new(limits),
            frame: AudioFrame::default(),
            output: Output::default(),
            generation: 0,
            phase: Phase::Open,
        }
    }
    /// A new stream: queued input goes and `decoder` takes over; `None` plays no audio
    /// ([`SessionState::Idle`]).
    pub fn load(&mut self, decoder: Option<Box<DynAudioDecoder>>) {
        self.reset();
        self.delay = decoder.as_ref().map_or(0, |decoder| decoder.delay());
        self.decoder = decoder;
    }
    /// Seek, flush or stop: queued audio is discarded and the decoder drops its signal
    /// history; the lane keeps its decoder and takes input again.
    pub fn stop(&mut self) {
        self.reset();
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.reset();
        }
    }
    fn reset(&mut self) {
        self.generation += 1;
        self.queue.clear();
        self.output.carry = None;
        self.phase = Phase::Open;
    }
    /// Increases with every [`load`](Self::load) and [`stop`](Self::stop).
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Queue one access unit with its caller PTS (`None` when unknown). Refused without a
    /// decoder or after a failure ([`FeedError::Idle`]), after
    /// [`end_of_stream`](Self::end_of_stream) or for an empty or oversized unit
    /// ([`FeedError::Invalid`]), and while the queue is full ([`FeedError::Backpressure`]:
    /// keep the unit and retry).
    pub fn feed(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), FeedError> {
        self.feed_discarding(au, pts, Discard::default())
    }
    /// [`feed`](Self::feed) a container packet's payload, whose decoded output then drops
    /// the samples its container marks for discarding ([`Discard::of`]).
    pub fn feed_discarding(
        &mut self,
        au: &[u8],
        pts: Option<i64>,
        discard: Discard,
    ) -> Result<(), FeedError> {
        self.phase.admit(self.decoder.is_some())?;
        self.queue.push_discarding(au, pts, discard)
    }
    /// No more input for this generation. Once every queued unit is decoded, the samples
    /// the decoder still holds back ([`AudioDecoder::flush`]) follow, and the lane reports
    /// [`SessionState::Ended`] when they are delivered.
    ///
    /// [`AudioDecoder::flush`]: triergon_codec::AudioDecoder::flush
    pub fn end_of_stream(&mut self) {
        if self.decoder.is_none() || self.phase != Phase::Open {
            return;
        }
        self.phase = Phase::Ending;
        if self.queue.is_empty() && self.output.carry.is_none() {
            let mut held = PcmBlock::default();
            self.drain(&mut held);
            if !held.samples.is_empty() {
                self.output.carry = Some(held);
            }
        }
    }
    /// Decode every queued unit that is due at `clock_ns` (PTS minus `lead_ns`
    /// reached, or unknown PTS) into `out`. A sample-rate change ends the block
    /// early so one block never mixes rates. Returns the units consumed.
    /// `lead_ns` applies as given; see [`DEFAULT_LEAD_NS`].
    pub fn pull(&mut self, clock_ns: Option<i64>, lead_ns: i64, out: &mut PcmBlock) -> usize {
        let Some(decoder) = self.decoder.as_mut() else {
            return 0;
        };
        if let Some(carry) = self.output.carry.take() {
            if out.samples.is_empty() {
                *out = carry;
            } else {
                self.output.carry = Some(carry);
                return 0;
            }
        }
        let due = |pts: Option<i64>| match (pts, clock_ns) {
            (Some(pts), Some(clock)) => clock >= pts.saturating_sub(lead_ns),
            _ => true,
        };
        let mut consumed = 0;
        // A failed lane's queue is empty.
        while self.output.carry.is_none()
            && let Some(unit) = self.queue.pop_if(due)
        {
            consumed += 1;
            match decoder.decode(&unit.data, &mut self.frame) {
                Ok(levels) => {
                    unit.discard.apply(&mut self.frame, &mut self.delay);
                    self.output.append(&self.frame, levels, unit.pts, out);
                }
                Err(error) => {
                    self.phase = Phase::Failed(error);
                    self.queue.clear();
                }
            }
        }
        if self.phase == Phase::Ending && self.queue.is_empty() && self.output.carry.is_none() {
            self.drain(out);
        }
        consumed
    }
    /// Append the samples the decoder still holds back to `out`, as they are: no discard
    /// and no start delay apply to them.
    fn drain(&mut self, out: &mut PcmBlock) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        self.phase = match decoder.flush(&mut self.frame) {
            Ok(()) => {
                self.output.append(&self.frame, None, None, out);
                Phase::Drained
            }
            Err(error) => Phase::Failed(error),
        };
    }
    /// Why the lane is or is not producing output.
    pub fn state(&self) -> SessionState {
        let pending = !self.queue.is_empty() || self.output.carry.is_some();
        self.phase.state(self.decoder.is_some(), pending)
    }
}

/// Decoded frames on their way into the caller's blocks.
#[derive(Default)]
struct Output {
    downmix: Downmix,
    /// Samples decoded at a new rate while the caller's block held another.
    carry: Option<PcmBlock>,
}

impl Output {
    /// Mix `frame` onto `out`, or into a carried block when `out` holds samples at another
    /// rate; `pts` becomes the block's when the frame starts it.
    fn append(
        &mut self,
        frame: &AudioFrame,
        levels: Option<DownmixLevels>,
        pts: Option<i64>,
        out: &mut PcmBlock,
    ) {
        if frame.samples_per_channel == 0 {
            return;
        }
        let target = if out.samples.is_empty() || out.sample_rate == frame.sample_rate {
            out
        } else {
            self.carry.get_or_insert_with(PcmBlock::default)
        };
        if target.samples.is_empty() {
            target.sample_rate = frame.sample_rate;
            target.pts = pts;
        }
        self.downmix
            .mix(frame, levels.unwrap_or_default(), &mut target.samples);
    }
}
