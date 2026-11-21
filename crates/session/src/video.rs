//! Video lane: Annex-B access units in, one clock-gated 1920x1080 RGBA picture out.
//!
//! Each picture keeps the timestamp its access unit carried through the
//! decoder's reordering. The lane holds at most one converted picture ahead of
//! the clock and publishes it once the clock reaches its PTS.
use crate::SessionState;
use crate::lane::Phase;
use crate::queue::{FeedError, QueueLimits, UnitQueue};
use std::sync::Arc;
use triergon_codec::DynVideoDecoder;
use triergon_pixel::letterbox::{CANVAS_BYTES, Scaler};

/// The current picture for the compositor. `sequence` increases for every
/// publication of this session object, across generations, so a texture cache
/// never mistakes a new stream's first picture for an old one.
#[derive(Clone, Debug)]
pub struct Published {
    pub sequence: u64,
    pub generation: u64,
    pub pts: Option<i64>,
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Debug)]
struct Ahead {
    pts: Option<i64>,
    rgba: Vec<u8>,
}

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
}

impl VideoSession {
    pub fn new(limits: QueueLimits) -> Self {
        Self {
            decoder: None,
            queue: UnitQueue::new(limits),
            scaler: Scaler::default(),
            ahead: None,
            spare: None,
            latest: None,
            generation: 0,
            sequence: 0,
            phase: Phase::Open,
        }
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
        if clear_picture {
            self.latest = None;
        }
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
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
                rgba.resize(CANVAS_BYTES, 0);
                self.scaler.letterbox(&view, &mut rgba);
                self.ahead = Some(Ahead {
                    pts: view.pts,
                    rgba,
                });
                continue;
            }
            match self.queue.pop() {
                Some(unit) => {
                    if let Err(error) = decoder.send(&unit.data, unit.pts) {
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
    pub fn latest(&self) -> Option<&Published> {
        self.latest.as_ref()
    }
    pub fn state(&self) -> SessionState {
        let pending = self.ahead.is_some() || !self.queue.is_empty();
        self.phase.state(self.decoder.is_some(), pending)
    }
}
