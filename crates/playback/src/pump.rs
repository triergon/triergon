//! Consumer-neutral producer/pump ownership. Accepted envelopes MOVE to the sink;
//! a blocked or failed submission returns the same source owner. No borrowed
//! native Feed/Load lifetime or device behavior is inferred by this contract.
use crate::source::Source;
use crate::{
    Output, Player, Progress, Result, SeekIntent, Selection, TrackKey,
    mp4::{Mp4DemuxSeekCommand, Mp4SyncSeekCommand},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    BudgetVec, Error, InputCursor, InputToken, SourceCapabilities, SourceId, Time, WorkBudget,
};

/// Names one feed a pump handed to its sink; the sink reports its completion with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedToken {
    scope: InputToken,
    ordinal: u64,
}
impl FeedToken {
    /// The feed's position in the pump's order.
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }
}
/// One player output on its way to a sink.
#[derive(Debug)]
pub struct Feed {
    token: FeedToken,
    output: Output,
}
impl Feed {
    /// The feed's token.
    pub fn token(&self) -> &FeedToken {
        &self.token
    }
    /// The output.
    pub fn output(&self) -> &Output {
        &self.output
    }
    /// Takes the output.
    pub fn into_output(self) -> Output {
        self.output
    }
}
/// A sink's answer to one feed.
#[derive(Debug)]
pub enum Submission {
    /// The sink owns the feed until it reports this token complete.
    Accepted(FeedToken),
    /// The sink is full: the same feed comes back for a later step.
    Blocked(Feed),
    /// The sink failed; the feed comes back.
    Failed {
        /// The feed the sink did not take.
        feed: Feed,
        /// Why the sink failed.
        error: Error,
    },
}
/// A consumer of player outputs, such as a decoder queue.
pub trait Sink {
    /// Accepted means this sink owns the envelope; a hardware adapter retains it
    /// independently until its actual callback/native borrow has retired.
    fn submit(&mut self, feed: Feed) -> Submission;
    /// The token of a feed that finished, if one did.
    fn completion(&mut self) -> Option<FeedToken>;
    /// Pauses or resumes the consumer.
    fn pause(&mut self, paused: bool) -> Result<()>;
    /// Scope changes fence callbacks. This must not free a still-borrowed native
    /// buffer; those owners remain in the sink's callback/retirement registry.
    fn reset(&mut self, intent: &SeekIntent) -> Result<()>;
    /// Stops the consumer.
    fn stop(&mut self);
}
/// Optional two-phase catalogue boundary for a sink with no outstanding feeds.
/// Prepare is prompt/bounded and may reserve resources, but cannot release or
/// change the old decoder/owners. Abandon restores that preparation on refusal.
/// Commit is infallible using the prepared resources and preserves the current
/// pause state; platform scheduling and
/// native lifetime proof remain the caller's implementation responsibility.
pub trait CatalogueSink: Sink {
    /// Prepares for the outputs of `next`, reserving what the change needs.
    fn prepare_catalogue(&mut self, next: &crate::Engine) -> Result<()>;
    /// Undoes `prepare_catalogue` after the change was refused.
    fn abandon_catalogue(&mut self);
    /// Switches to the new catalogue, whose outputs begin at `boundary`.
    fn commit_catalogue(&mut self, boundary: crate::mp4::catalogue::CatalogueBoundary);
}
/// One step of a [`Pump`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PumpStep {
    /// The work budget ran out or the step made progress: step again.
    Yield,
    /// The player produced the next feed.
    Produced,
    /// The sink accepted a feed.
    Fed,
    /// The sink is full: step again once it completes a feed.
    SinkFull,
    /// The in-flight bound is reached and no feed completed.
    InFlightFull,
    /// The sink completed a feed the pump no longer tracks.
    StaleCompletion,
    /// The pump is paused.
    Paused,
    /// The source ended and every feed completed.
    Ended,
    /// The pump stopped.
    Stopped,
}
enum CatalogueChange<'a> {
    Replace(Selection),
    ProgressiveSeek(crate::mp4::catalogue::RangeSeek<'a>),
}

/// Moves a player's outputs into a sink, with a bounded number of feeds in flight.
#[derive(Debug)]
pub struct Pump<S: Source, T: Sink> {
    player: Option<Player<S>>,
    sink: Option<T>,
    identity: InputCursor,
    credits: BudgetVec<Option<FeedToken>>,
    pending: Option<Feed>,
    ordinal: u64,
    paused: bool,
    stopped: bool,
    ended: bool,
}
impl<S: Source, T: Sink> Pump<S, T> {
    /// A pump with up to `maximum_in_flight` feeds in flight. Fails with `Invalid`
    /// unless that is 1 to 64.
    pub fn new(player: Player<S>, sink: T, maximum_in_flight: usize) -> Result<Self> {
        if maximum_in_flight == 0 || maximum_in_flight > 64 {
            return Err(Error::Invalid("pump in-flight policy"));
        }
        let pool = player.budget();
        // Control identity only: never emitted as a physical source/native field.
        let identity = InputCursor::new(
            pool,
            SourceId(0),
            SourceCapabilities {
                length: Some(0),
                random_access: true,
            },
            1,
        )?;
        let mut credits = BudgetVec::new(pool);
        credits.try_reserve_exact(maximum_in_flight)?;
        for _ in 0..maximum_in_flight {
            credits.try_push(None).map_err(|e| e.error)?;
        }
        Ok(Self {
            player: Some(player),
            sink: Some(sink),
            identity,
            credits,
            pending: None,
            ordinal: 0,
            paused: false,
            stopped: false,
            ended: false,
        })
    }
    /// The player.
    pub fn player(&self) -> Option<&Player<S>> {
        self.player.as_ref()
    }
    /// The sink.
    pub fn sink(&self) -> Option<&T> {
        self.sink.as_ref()
    }
    /// The sink, mutably.
    pub fn sink_mut(&mut self) -> Option<&mut T> {
        self.sink.as_mut()
    }
    /// The number of feeds the sink holds.
    pub fn in_flight(&self) -> usize {
        self.credits
            .as_slice()
            .iter()
            .filter(|s| s.is_some())
            .count()
    }
    /// The feed the sink has not accepted yet, if any.
    pub fn pending(&self) -> Option<&Feed> {
        self.pending.as_ref()
    }
    /// Pauses or resumes the sink, then the player. A sink failure stops the pump.
    /// Fails with `Closed` once the pump stopped or the player ended.
    pub fn pause(&mut self, paused: bool) -> Result<()> {
        if self.stopped {
            return Err(Error::Closed);
        }
        if !matches!(
            self.player.as_ref().ok_or(Error::Closed)?.state(),
            crate::State::Running | crate::State::Paused
        ) {
            return Err(Error::Closed);
        }
        if let Err(error) = self.sink.as_mut().ok_or(Error::Closed)?.pause(paused) {
            self.stop();
            return Err(error);
        }
        self.player.as_mut().ok_or(Error::Closed)?.pause(paused)?;
        self.paused = paused;
        Ok(())
    }
    /// Seeks the player to `target` on `track` with `selection`, then resets the sink
    /// and forgets the feeds in flight; `false` when the player had no work to start
    /// the seek. A sink failure stops the pump.
    pub fn seek_and_select(
        &mut self,
        track: TrackKey,
        target: Time,
        selection: Selection,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<bool> {
        if c.load(Ordering::Relaxed) {
            self.stop();
            return Err(Error::Cancelled);
        }
        if self.stopped {
            return Err(Error::Closed);
        }
        let player = self.player.as_mut().ok_or(Error::Closed)?;
        if !player.seek_and_select(track, target, selection, w, c)? {
            return Ok(false);
        }
        self.identity.reset(0)?;
        let intent = player
            .seek_intent()
            .ok_or(Error::Invalid("missing accepted seek intent"))?;
        // Source player owns its exact selected clock. This intent is a sink
        // control request, not replacement source timing or a native seek receipt.
        if let Err(error) = self.sink.as_mut().ok_or(Error::Closed)?.reset(&intent) {
            self.stop();
            return Err(error);
        }
        self.pending = None;
        for credit in self.credits.as_mut_slice() {
            *credit = None;
        }
        self.ended = false;
        Ok(true)
    }
    /// Old accepted/blocked Feed owners must retire before the sink boundary.
    /// Refusal never drops such an owner or advances the active player/source.
    pub fn replace_mp4_catalogue(
        &mut self,
        next: &mut Option<crate::mp4::catalogue::Mp4Catalogue<S>>,
        selection: Selection,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Option<crate::mp4::catalogue::RetiredCatalogue>>
    where
        T: CatalogueSink,
    {
        self.commit_catalogue_change(next, CatalogueChange::Replace(selection), w, c)
    }

    /// Promote progressive playback to a seek on the same checked source.
    /// Both queued sink ownership and stale callbacks use the catalogue fence.
    pub fn apply_mp4_progressive_sync_seek(
        &mut self,
        next: &mut Option<crate::mp4::catalogue::Mp4Catalogue<S>>,
        command: &Mp4SyncSeekCommand,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Option<crate::mp4::catalogue::RetiredCatalogue>>
    where
        T: CatalogueSink,
    {
        self.commit_catalogue_change(next, CatalogueChange::ProgressiveSeek(command.seek()), w, c)
    }

    /// Applies a demux-clock seek that switches a progressive player to `next`, a
    /// complete range index of the same entity, fencing earlier sink callbacks.
    pub fn apply_mp4_progressive_demux_seek(
        &mut self,
        next: &mut Option<crate::mp4::catalogue::Mp4Catalogue<S>>,
        command: &Mp4DemuxSeekCommand,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Option<crate::mp4::catalogue::RetiredCatalogue>>
    where
        T: CatalogueSink,
    {
        self.commit_catalogue_change(next, CatalogueChange::ProgressiveSeek(command.seek()), w, c)
    }

    fn commit_catalogue_change(
        &mut self,
        next: &mut Option<crate::mp4::catalogue::Mp4Catalogue<S>>,
        change: CatalogueChange<'_>,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<Option<crate::mp4::catalogue::RetiredCatalogue>>
    where
        T: CatalogueSink,
    {
        if c.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.stopped {
            return Err(Error::Closed);
        }
        if w.remaining() == 0 {
            return Ok(None);
        }
        let outstanding = self.in_flight() + usize::from(self.pending.is_some());
        if outstanding != 0 {
            return Err(Error::Backpressure(triergon_core::Backpressure {
                requested: outstanding,
                available: 0,
            }));
        }
        // Construct the new callback fence before changing the player or sink.
        let identity = InputCursor::new(
            self.player.as_ref().ok_or(Error::Closed)?.budget(),
            SourceId(0),
            SourceCapabilities {
                length: Some(0),
                random_access: true,
            },
            1,
        )?;
        let player = self.player.as_mut().ok_or(Error::Closed)?;
        let transaction = match change {
            CatalogueChange::Replace(selection) => {
                player.prepare_mp4_catalogue(next, selection, w, c)?
            }
            CatalogueChange::ProgressiveSeek(seek) => {
                player.prepare_progressive_commit(next, seek, w, c)?
            }
        };
        let Some(transaction) = transaction else {
            return Ok(None);
        };
        let sink = self.sink.as_mut().ok_or(Error::Closed)?;
        if let Err(error) = sink.prepare_catalogue(transaction.next_engine()?) {
            sink.abandon_catalogue();
            return Err(error);
        }
        let (retired, boundary) = match transaction.commit_with_boundary() {
            Ok(result) => result,
            Err(error) => {
                sink.abandon_catalogue();
                return Err(error);
            }
        };
        sink.commit_catalogue(boundary);
        self.identity = identity;
        self.ended = false;
        let intent = match change {
            CatalogueChange::Replace(_) => None,
            CatalogueChange::ProgressiveSeek(seek) => Some(seek.intent),
        };
        if let Some(intent) = intent
            && let Err(error) = sink.reset(&intent)
        {
            self.stop();
            return Err(error);
        }
        Ok(Some(retired))
    }
    fn complete(&mut self, w: &mut WorkBudget) -> Result<PumpStep> {
        if !w.try_charge(1) {
            return Ok(PumpStep::Yield);
        }
        let Some(token) = self.sink.as_mut().ok_or(Error::Closed)?.completion() else {
            return Ok(PumpStep::InFlightFull);
        };
        if let Some(slot) = self
            .credits
            .as_mut_slice()
            .iter_mut()
            .find(|x| x.as_ref() == Some(&token))
        {
            *slot = None;
            Ok(PumpStep::Yield)
        } else {
            Ok(PumpStep::StaleCompletion)
        }
    }
    /// Completes, feeds or produces within `w`. Cancellation and a player failure stop
    /// the pump.
    pub fn step(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<PumpStep> {
        if c.load(Ordering::Relaxed) {
            self.stop();
            return Err(Error::Cancelled);
        }
        if self.stopped {
            return Ok(PumpStep::Stopped);
        }
        if self.paused {
            return Ok(PumpStep::Paused);
        }
        if w.remaining() == 0 {
            return Ok(PumpStep::Yield);
        }
        if self.ended {
            return if self.in_flight() == 0 {
                Ok(PumpStep::Ended)
            } else {
                self.complete(w)
            };
        }
        if let Some(feed) = self.pending.as_ref() {
            let player = self.player.as_ref().ok_or(Error::Closed)?;
            if !player.current(&feed.output) {
                self.pending = None;
                return Err(Error::Invalid("retired producer output"));
            }
            let Some(slot) = self.credits.as_slice().iter().position(Option::is_none) else {
                return self.complete(w);
            };
            if !w.try_charge(1) {
                return Ok(PumpStep::Yield);
            }
            let token = feed.token.clone();
            let feed = self.pending.take().ok_or(Error::Closed)?;
            match self.sink.as_mut().ok_or(Error::Closed)?.submit(feed) {
                Submission::Accepted(receipt) if receipt == token => {
                    self.credits.as_mut_slice()[slot] = Some(token);
                    return Ok(PumpStep::Fed);
                }
                Submission::Accepted(_) => {
                    self.stop();
                    return Err(Error::Invalid("foreign sink acceptance token"));
                }
                Submission::Blocked(feed) if feed.token == token => {
                    self.pending = Some(feed);
                    return Ok(PumpStep::SinkFull);
                }
                Submission::Blocked(_) => {
                    self.stop();
                    return Err(Error::Invalid("foreign returned sink owner"));
                }
                Submission::Failed { feed, error } => {
                    self.pending = Some(feed);
                    self.stop();
                    return Err(error);
                }
            }
        }
        if self.in_flight() == self.credits.len() {
            return self.complete(w);
        }
        let player = self.player.as_mut().ok_or(Error::Closed)?;
        if let Some(output) = player.pop() {
            if !w.try_charge(1) {
                return Err(Error::Invalid("work admission changed after pop"));
            }
            self.ordinal = self.ordinal.checked_add(1).ok_or(Error::Overflow)?;
            self.pending = Some(Feed {
                token: FeedToken {
                    scope: self.identity.token(),
                    ordinal: self.ordinal,
                },
                output,
            });
            return Ok(PumpStep::Produced);
        }
        match player.advance_ready(w, c) {
            Ok(Progress::Ended) => {
                self.ended = true;
                Ok(PumpStep::Yield)
            }
            Ok(Progress::Paused) => Ok(PumpStep::Paused),
            Ok(Progress::Stopped) => {
                self.stop();
                Ok(PumpStep::Stopped)
            }
            Ok(_) => Ok(PumpStep::Yield),
            Err(e) => {
                self.stop();
                Err(e)
            }
        }
    }
    /// Stops the player, then the sink, and drops the feeds; once.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        // Source abort/quiescence/close precede sink stop and local owner release.
        if let Some(player) = self.player.as_mut() {
            player.stop();
        }
        if let Some(sink) = self.sink.as_mut() {
            sink.stop();
        }
        self.pending = None;
        for s in self.credits.as_mut_slice() {
            *s = None;
        }
        self.stopped = true;
    }
    /// Stops the pump and returns its sink.
    pub fn into_sink(mut self) -> Result<T> {
        self.stop();
        self.player = None;
        self.sink.take().ok_or(Error::Closed)
    }
}
impl<S: Source, T: Sink> Drop for Pump<S, T> {
    fn drop(&mut self) {
        self.stop();
    }
}
