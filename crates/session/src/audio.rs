//! Audio lane: compressed frames in, clock-paced stereo s16 PCM out.
//!
//! Each access unit is held until the sink clock is within `lead` of its PTS;
//! `pull` then decodes exactly those units and returns their stereo samples for
//! the caller's device writer. A paused clock
//! therefore yields nothing, and flush/stop discard everything queued.
use crate::Discard;
use crate::SessionState;
use crate::mix::Downmix;
use crate::queue::{FeedError, QueueLimits, UnitQueue};
use triergon_codec::{AudioDecoder, AudioFrame};

/// Interleaved stereo s16 at `sample_rate`; `pts` is the first unit's caller PTS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcmBlock {
    pub sample_rate: u32,
    pub pts: Option<i64>,
    pub samples: Vec<i16>,
}

/// The usual audio lead for [`AudioSession::pull`]: 120 ms.
pub const DEFAULT_LEAD_NS: i64 = 120_000_000;

pub struct AudioSession {
    decoder: Option<Box<dyn AudioDecoder>>,
    /// Samples of the decoder's start delay not discarded yet.
    delay: usize,
    queue: UnitQueue,
    frame: AudioFrame,
    downmix: Downmix,
    // Samples decoded at a new rate while the caller's block held another.
    carry: Option<PcmBlock>,
    generation: u64,
    ended: bool,
    failure: Option<String>,
}

impl AudioSession {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            decoder: None,
            delay: 0,
            queue: UnitQueue::new(limits),
            frame: AudioFrame::default(),
            downmix: Downmix::default(),
            carry: None,
            generation: 0,
            ended: false,
            failure: None,
        }
    }
    /// A new stream; `None` plays no audio.
    pub fn load(&mut self, decoder: Option<Box<dyn AudioDecoder>>) {
        self.reset();
        self.delay = decoder.as_ref().map_or(0, |decoder| decoder.delay());
        self.decoder = decoder;
    }
    /// Flush and stop both end the fed timeline: queued audio is discarded.
    pub fn stop(&mut self) {
        self.reset();
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.reset();
        }
    }
    fn reset(&mut self) {
        self.generation += 1;
        self.queue.clear();
        self.carry = None;
        self.ended = false;
        self.failure = None;
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn feed(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), FeedError> {
        if self.decoder.is_none() || self.failure.is_some() {
            return Err(FeedError::Idle);
        }
        if self.ended {
            return Err(FeedError::Invalid);
        }
        self.queue.push(au, pts)
    }
    /// [`feed`](Self::feed) a container packet's payload, whose decoded output then drops
    /// the samples its container marks for discarding ([`Discard::of`]).
    pub fn feed_discarding(
        &mut self,
        au: &[u8],
        pts: Option<i64>,
        discard: Discard,
    ) -> Result<(), FeedError> {
        if self.decoder.is_none() || self.failure.is_some() {
            return Err(FeedError::Idle);
        }
        if self.ended {
            return Err(FeedError::Invalid);
        }
        self.queue.push_discarding(au, pts, discard)
    }
    pub fn end_of_stream(&mut self) {
        if self.decoder.is_some() {
            self.ended = true;
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
        if let Some(carry) = self.carry.take() {
            if out.samples.is_empty() {
                *out = carry;
            } else {
                self.carry = Some(carry);
                return 0;
            }
        }
        let mut consumed = 0;
        while self.failure.is_none() && self.carry.is_none() {
            let Some(pts) = self.queue.front_pts() else {
                break;
            };
            if let (Some(pts), Some(clock)) = (pts, clock_ns)
                && clock < pts.saturating_sub(lead_ns)
            {
                break;
            }
            let unit = self.queue.pop().expect("front unit");
            let result = decoder.decode(&unit.data, &mut self.frame);
            if result.is_ok() {
                unit.discard.apply(&mut self.frame, &mut self.delay);
            }
            consumed += 1;
            match result {
                Err(error) => {
                    self.failure = Some(error.0);
                    self.queue.clear();
                }
                Ok(_) if self.frame.samples_per_channel == 0 => {}
                Ok(levels) => {
                    let target =
                        if out.samples.is_empty() || out.sample_rate == self.frame.sample_rate {
                            &mut *out
                        } else {
                            self.carry.get_or_insert_with(PcmBlock::default)
                        };
                    if target.samples.is_empty() {
                        target.sample_rate = self.frame.sample_rate;
                        target.pts = unit.pts;
                    }
                    let levels = levels.unwrap_or_default();
                    self.downmix.mix(&self.frame, levels, &mut target.samples);
                }
            }
        }
        consumed
    }
    pub fn state(&self) -> SessionState {
        match (&self.decoder, &self.failure) {
            (None, _) => SessionState::Idle,
            (_, Some(message)) => SessionState::Failed(message.clone()),
            _ if !self.queue.is_empty() || self.carry.is_some() => SessionState::Running,
            _ if self.ended => SessionState::Ended,
            _ => SessionState::Starved,
        }
    }
}
