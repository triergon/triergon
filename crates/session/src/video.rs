//! Video lane: Annex-B access units in, one clock-gated RGBA picture out, letterboxed into
//! the lane's canvas (1920x1080 unless chosen otherwise).
//!
//! Each picture keeps the timestamp its access unit carried through the
//! decoder's reordering. The lane holds at most one converted picture ahead of
//! the clock and publishes it once the clock reaches its PTS.
use crate::SessionState;
use crate::lane::{OnRefusal, Phase, Refusals};
use crate::queue::{FeedError, QueueLimits, UnitQueue};
use std::sync::Arc;
use triergon_codec::{DynVideoDecoder, Error};
use triergon_pixel::letterbox::{Canvas, Scaler};

/// The current picture for the compositor. `sequence` increases for every
/// publication of this session object, across generations, so a texture cache
/// never mistakes a new stream's first picture for an old one.
#[derive(Clone, Debug)]
pub struct Published {
    /// Increases with every picture this session publishes.
    pub sequence: u64,
    /// The session's [`generation`](VideoSession::generation) when it was published.
    pub generation: u64,
    /// The caller PTS of the picture's access unit.
    pub pts: Option<i64>,
    /// The letterboxed picture: the session's [`Canvas`] of RGBA, rows top to bottom, no
    /// padding.
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct Ahead {
    pts: Option<i64>,
    rgba: Vec<u8>,
}

/// The video lane of a playback session: a bounded queue of Annex-B access units, a
/// decoder, and the letterboxed picture the caller's clock has reached.
pub struct VideoSession {
    decoder: Option<Box<DynVideoDecoder>>,
    queue: UnitQueue,
    scaler: Scaler,
    ahead: Option<Ahead>,
    spare: Option<Vec<u8>>,
    latest: Option<Published>,
    generation: u64,
    sequence: u64,
    phase: Phase,
    refusals: Refusals,
}

impl core::fmt::Debug for VideoSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VideoSession")
            .field("state", &self.state())
            .field("generation", &self.generation)
            .field("sequence", &self.sequence)
            .field("refusals", &self.refusals)
            .finish_non_exhaustive()
    }
}

impl VideoSession {
    /// An idle lane whose queue holds at most `limits`, publishing pictures on the
    /// default 1920x1080 canvas. A refused access unit fails it ([`OnRefusal::Fail`]).
    pub fn new(limits: QueueLimits) -> Self {
        Self::with_canvas(limits, Canvas::default())
    }
    /// [`new`](Self::new) with pictures letterboxed into `canvas`.
    pub fn with_canvas(limits: QueueLimits, canvas: Canvas) -> Self {
        Self {
            decoder: None,
            queue: UnitQueue::new(limits),
            scaler: Scaler::new(canvas),
            ahead: None,
            spare: None,
            latest: None,
            generation: 0,
            sequence: 0,
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
    /// A new stream: old input, decoder state and picture go.
    pub fn load(&mut self, decoder: Box<DynVideoDecoder>) {
        self.reset(true);
        self.decoder = Some(decoder);
    }
    /// Seek/flush: the fed timeline restarts; the last picture stays on screen.
    pub fn flush(&mut self) {
        self.reset(false);
        if let Some(decoder) = self.decoder.as_mut() {
            decoder.reset();
        }
    }
    /// Unload/destroy: no stream and no picture.
    pub fn stop(&mut self) {
        self.reset(true);
        self.decoder = None;
    }
    fn reset(&mut self, clear_picture: bool) {
        self.generation += 1;
        self.queue.clear();
        if let Some(ahead) = self.ahead.take() {
            self.spare = Some(ahead.rgba);
        }
        self.phase = Phase::Open;
        self.refusals.clear();
        if clear_picture {
            self.latest = None;
        }
    }
    /// The canvas its pictures are letterboxed into.
    pub fn canvas(&self) -> Canvas {
        self.scaler.canvas()
    }
    /// Increases with every [`load`](Self::load), [`flush`](Self::flush) and
    /// [`stop`](Self::stop).
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Queue one Annex-B access unit with its caller PTS (`None` when unknown). Refused as
    /// [`AudioSession::feed`](crate::AudioSession::feed) refuses.
    pub fn feed(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), FeedError> {
        self.phase.admit(self.decoder.is_some())?;
        self.queue.push(au, pts)
    }
    /// No more input for this generation: the decoder drains and the lane
    /// reports `Ended` once every picture has been published.
    pub fn end_of_stream(&mut self) {
        if self.decoder.is_some() && self.phase == Phase::Open {
            self.phase = Phase::Ending;
        }
    }
    /// Bounded work: decode until one picture waits ahead of the clock or the
    /// input is exhausted, then publish it if `clock_ns` has reached it.
    /// Returns true when a new picture was published.
    pub fn step(&mut self, clock_ns: Option<i64>) -> bool {
        self.fill_ahead();
        let due = |ahead: &mut Ahead| {
            ahead
                .pts
                .is_none_or(|pts| clock_ns.is_none_or(|clock| clock >= pts))
        };
        let Some(ahead) = self.ahead.take_if(due) else {
            return false;
        };
        let previous = self.latest.take();
        self.sequence += 1;
        self.latest = Some(Published {
            sequence: self.sequence,
            generation: self.generation,
            pts: ahead.pts,
            rgba: Arc::new(ahead.rgba),
        });
        // Reuse the superseded buffer once no compositor clone still holds it.
        if let Some(old) = previous.and_then(|p| Arc::try_unwrap(p.rgba).ok()) {
            self.spare = Some(old);
        }
        true
    }
    fn fill_ahead(&mut self) {
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        while self.ahead.is_none() && !matches!(self.phase, Phase::Failed(_)) {
            if let Some(view) = decoder.receive() {
                let mut rgba = self.spare.take().unwrap_or_default();
                rgba.resize(self.scaler.canvas().bytes(), 0);
                self.scaler.letterbox(&view, &mut rgba);
                self.ahead = Some(Ahead {
                    pts: view.pts,
                    rgba,
                });
                continue;
            }
            match self.queue.pop() {
                Some(unit) => {
                    if let Err(error) = decoder.send(&unit.data, unit.pts)
                        && self.refusals.refuse(error) == OnRefusal::Fail
                    {
                        self.phase = Phase::Failed(error);
                        self.queue.clear();
                    }
                }
                None if self.phase == Phase::Ending => {
                    decoder.flush();
                    self.phase = Phase::Drained;
                }
                None => return,
            }
        }
    }
    /// The picture on screen: the last one published, until a load or stop.
    pub fn latest(&self) -> Option<&Published> {
        self.latest.as_ref()
    }
    /// Why the lane is or is not producing output.
    pub fn state(&self) -> SessionState {
        let pending = self.ahead.is_some() || !self.queue.is_empty();
        self.phase.state(self.decoder.is_some(), pending)
    }
    /// Access units the decoder refused since the last [`load`](Self::load),
    /// [`flush`](Self::flush) or [`stop`](Self::stop).
    pub fn refusals(&self) -> u64 {
        self.refusals.count
    }
    /// The decoder's latest refusal since then.
    pub fn last_refusal(&self) -> Option<Error> {
        self.refusals.last
    }
}
