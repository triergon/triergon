//! Audio lane: compressed frames in, clock-paced PCM out, by default interleaved stereo
//! s16 ([`PcmForm`]).
//!
//! Each access unit is held until the sink clock is within `lead` of its PTS;
//! `pull` then decodes exactly those units and returns their samples for
//! the caller's device writer. A paused clock
//! therefore yields nothing, and flush/stop discard everything queued.
use crate::Discard;
use crate::SessionState;
use crate::lane::{OnRefusal, Phase, Refusals};
use crate::mix::Downmix;
use crate::queue::{FeedError, QueueLimits, UnitQueue};
use triergon_codec::{AudioFrame, ChannelLayout, DownmixLevels, DynAudioDecoder, Error};

/// One block of output at one sample rate, its samples in the lane's [`PcmForm`]: by
/// default interleaved stereo s16.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcmBlock<S = Vec<i16>> {
    /// Samples per second per channel; meaningful while `samples` is not empty.
    pub sample_rate: u32,
    /// The first retained sample's PTS in nanoseconds, from its caller PTS and leading
    /// trim. `None` when the caller PTS is unknown or the block holds only drain samples.
    pub pts: Option<i64>,
    /// The samples; for the default form left and right samples in turn.
    pub samples: S,
}

/// How an audio lane turns decoded frames into the samples of its [`PcmBlock`]s.
/// [`Downmix`] (interleaved stereo s16, the default) and [`Planar`] (the decoder's planes
/// as they are) are this crate's; a caller may implement another.
pub trait PcmForm {
    /// The samples of one block.
    type Samples: Default;
    /// Whether `samples` holds none.
    fn is_empty(samples: &Self::Samples) -> bool;
    /// Whether `frame` may follow `samples` in one block; a frame at another sample rate
    /// never does. The default takes every frame.
    fn continues(samples: &Self::Samples, frame: &AudioFrame) -> bool {
        let _ = (samples, frame);
        true
    }
    /// Append the samples of `frame` to `samples`; `levels` are the stream's downmix
    /// levels (their default when it carries none).
    fn append(&mut self, frame: &AudioFrame, levels: DownmixLevels, samples: &mut Self::Samples);
}

impl PcmForm for Downmix {
    type Samples = Vec<i16>;

    #[inline]
    fn is_empty(samples: &Vec<i16>) -> bool {
        samples.is_empty()
    }
    #[inline]
    fn append(&mut self, frame: &AudioFrame, levels: DownmixLevels, samples: &mut Vec<i16>) {
        self.mix(frame, levels, samples);
    }
}

/// The decoder's planes as they are: `f32` at full scale 1.0, one plane per speaker, no
/// downmix and no conversion. A block holds one layout; a frame of another starts the
/// next block.
#[derive(Clone, Copy, Debug, Default)]
pub struct Planar;

/// The samples of a [`Planar`] block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Planes {
    /// The speaker of each plane.
    pub layout: ChannelLayout,
    /// One plane per speaker of `layout`, all of one length.
    pub planes: Vec<Vec<f32>>,
}

impl Planes {
    /// Samples in each plane.
    pub fn samples_per_channel(&self) -> usize {
        self.planes.first().map_or(0, Vec::len)
    }
    /// Drop every sample, keeping the planes' allocations.
    pub fn clear(&mut self) {
        for plane in &mut self.planes {
            plane.clear();
        }
    }
}

impl PcmForm for Planar {
    type Samples = Planes;

    fn is_empty(samples: &Planes) -> bool {
        samples.samples_per_channel() == 0
    }
    fn continues(samples: &Planes, frame: &AudioFrame) -> bool {
        samples.layout == frame.layout
    }
    fn append(&mut self, frame: &AudioFrame, _: DownmixLevels, samples: &mut Planes) {
        if Self::is_empty(samples) {
            samples.layout = frame.layout;
            samples
                .planes
                .resize_with(frame.layout.channels(), Vec::new);
        }
        for (plane, decoded) in samples.planes.iter_mut().zip(&frame.planes) {
            plane.extend_from_slice(&decoded[..frame.samples_per_channel]);
        }
    }
}

/// The usual audio lead for [`AudioSession::pull`]: 120 ms.
pub const DEFAULT_LEAD_NS: i64 = 120_000_000;

/// The audio lane of a playback session: a bounded queue of compressed access units, a
/// decoder, and its output in the form `F` (a stereo downmix unless chosen otherwise),
/// paced by the caller's clock.
pub struct AudioSession<F: PcmForm = Downmix> {
    decoder: Option<Box<DynAudioDecoder>>,
    /// Samples of the decoder's start delay not discarded yet.
    delay: usize,
    queue: UnitQueue,
    frame: AudioFrame,
    output: Output<F>,
    generation: u64,
    phase: Phase,
    refusals: Refusals,
}

impl<F: PcmForm> core::fmt::Debug for AudioSession<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioSession")
            .field("state", &self.state())
            .field("generation", &self.generation)
            .field("refusals", &self.refusals)
            .finish_non_exhaustive()
    }
}

impl AudioSession {
    /// An idle lane whose queue holds at most `limits`, delivering interleaved stereo s16.
    /// A refused access unit fails it ([`OnRefusal::Fail`]).
    pub fn new(limits: QueueLimits) -> Self {
        Self::with_form(limits, Downmix::default())
    }
}

impl<F: PcmForm> AudioSession<F> {
    /// [`new`](AudioSession::new) delivering its samples in `form`.
    pub fn with_form(limits: QueueLimits, form: F) -> Self {
        Self {
            decoder: None,
            delay: 0,
            queue: UnitQueue::new(limits),
            frame: AudioFrame::default(),
            output: Output { form, carry: None },
            generation: 0,
            phase: Phase::Open,
            refusals: Refusals::default(),
        }
    }
    /// This lane, doing what `policy` says when its decoder refuses an access unit.
    #[must_use]
    pub fn on_refusal(mut self, policy: OnRefusal) -> Self {
        self.refusals.policy = policy;
        self
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
        self.refusals.clear();
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
            if !F::is_empty(&held.samples) {
                self.output.carry = Some(held);
            }
        }
    }
    /// Decode every queued unit that is due at `clock_ns` (PTS minus `lead_ns`
    /// reached, or unknown PTS) into `out`. A sample-rate change, or a frame the form
    /// does not continue a block with, ends the block early so one block never mixes
    /// rates. Returns the units consumed. `lead_ns` applies as given; see
    /// [`DEFAULT_LEAD_NS`].
    pub fn pull(
        &mut self,
        clock_ns: Option<i64>,
        lead_ns: i64,
        out: &mut PcmBlock<F::Samples>,
    ) -> usize {
        let Some(decoder) = self.decoder.as_mut() else {
            return 0;
        };
        if let Some(carry) = self.output.carry.take() {
            if F::is_empty(&out.samples) {
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
                    let skipped = unit.discard.apply(&mut self.frame, &mut self.delay);
                    self.output
                        .append(&self.frame, levels, unit.pts, skipped, out);
                }
                Err(error) => {
                    if self.refusals.refuse(error) == OnRefusal::Fail {
                        self.phase = Phase::Failed(error);
                        self.queue.clear();
                    }
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
    fn drain(&mut self, out: &mut PcmBlock<F::Samples>) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        self.phase = match decoder.flush(&mut self.frame) {
            Ok(()) => {
                self.output.append(&self.frame, None, None, 0, out);
                Phase::Drained
            }
            Err(error) => match self.refusals.refuse(error) {
                OnRefusal::Fail => Phase::Failed(error),
                OnRefusal::Skip => Phase::Drained,
            },
        };
    }
    /// Why the lane is or is not producing output.
    pub fn state(&self) -> SessionState {
        let pending = !self.queue.is_empty() || self.output.carry.is_some();
        self.phase.state(self.decoder.is_some(), pending)
    }
    /// Access units the decoder refused since the last [`load`](Self::load) or
    /// [`stop`](Self::stop), the end-of-stream drain included.
    pub fn refusals(&self) -> u64 {
        self.refusals.count
    }
    /// The decoder's latest refusal since then.
    pub fn last_refusal(&self) -> Option<Error> {
        self.refusals.last
    }
}

/// Decoded frames on their way into the caller's blocks.
struct Output<F: PcmForm> {
    form: F,
    /// Samples decoded at a new rate, or that the form does not continue the caller's
    /// block with.
    carry: Option<PcmBlock<F::Samples>>,
}

impl<F: PcmForm> Output<F> {
    /// Append `frame` to `out`, or to a carried block when `out` holds samples it does not
    /// continue; `pts` becomes the block's when the frame starts it.
    fn append(
        &mut self,
        frame: &AudioFrame,
        levels: Option<DownmixLevels>,
        pts: Option<i64>,
        skipped: usize,
        out: &mut PcmBlock<F::Samples>,
    ) {
        if frame.samples_per_channel == 0 {
            return;
        }
        let continues = F::is_empty(&out.samples)
            || (out.sample_rate == frame.sample_rate && F::continues(&out.samples, frame));
        let target = if continues {
            out
        } else {
            self.carry.get_or_insert_with(PcmBlock::default)
        };
        if F::is_empty(&target.samples) {
            target.sample_rate = frame.sample_rate;
            target.pts = if skipped == 0 {
                pts
            } else {
                pts.map(|pts| retained_pts(pts, skipped, frame.sample_rate))
            };
        }
        self.form
            .append(frame, levels.unwrap_or_default(), &mut target.samples);
    }
}

/// Advance the integral caller clock by the leading trim, rounded to the nearest ns.
#[cold]
fn retained_pts(pts: i64, skipped: usize, sample_rate: u32) -> i64 {
    let rate = i128::from(sample_rate.max(1));
    let ns = (skipped as i128 * 1_000_000_000 + rate / 2) / rate;
    i64::try_from(i128::from(pts) + ns).unwrap_or(i64::MAX)
}
