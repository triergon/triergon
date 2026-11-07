//! Explicit, transactional replacement of a checked MP4 source catalogue.
//! A second in-band moov is not a source discontinuity. The caller supplies a
//! freshly opened entity and keeps transport/entity validation outside this crate.
use crate::{
    Engine, Output, Player, RawEvent, Result, SeekIntent, Selection, Source,
    engines::Mp4RangeLanding,
    io::SourceOwner,
    mp4_open::{Movie, OpenedMp4},
    mp4_plan::PreparedMp4,
    progressive::ProgressiveLimits,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, InputRequest, SourceId};

#[derive(Clone, Copy, Debug)]
pub enum Mp4CatalogueMode {
    Indexed,
    Progressive(ProgressiveLimits),
    /// Explicit native view of a nonseekable MP4 starting with front metadata.
    NativeTerminalV1(ProgressiveLimits),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogueBoundary {
    pub source: SourceId,
    pub input_generation: u64,
    pub delivery_epoch: u64,
}

/// Only this constructor can create a candidate. Its engine has never emitted
/// a packet or advanced a delivery/configuration cursor. All source facts and
/// native-view refusals come from the existing checked open/index operations.
pub struct Mp4Catalogue<S: Source> {
    pub(crate) engine: Engine,
    pub(crate) source: SourceOwner<S>,
}
impl<S: Source> Mp4Catalogue<S> {
    pub fn new(
        opened: OpenedMp4<S>,
        plan: PreparedMp4,
        maximum_packet: usize,
        mode: Mp4CatalogueMode,
    ) -> Result<Self> {
        Self::new_with_movie(opened, plan, maximum_packet, mode).map(|(catalogue, _)| catalogue)
    }

    /// Retain the complete source tables for a native range seek.
    pub fn new_with_movie(
        opened: OpenedMp4<S>,
        plan: PreparedMp4,
        maximum_packet: usize,
        mode: Mp4CatalogueMode,
    ) -> Result<(Self, Movie)> {
        let engine = match mode {
            Mp4CatalogueMode::Indexed => Engine::mp4(opened.input, plan, maximum_packet)?,
            Mp4CatalogueMode::Progressive(limits) => {
                Engine::mp4_progressive(opened.input, plan, maximum_packet, limits)?
            }
            Mp4CatalogueMode::NativeTerminalV1(limits) => {
                Engine::mp4_native_terminal_v1(opened.input, plan, maximum_packet, limits)?
            }
        };
        Ok((
            Self {
                engine,
                source: opened.source,
            },
            opened.movie,
        ))
    }
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
    pub fn source(&self) -> SourceId {
        self.engine.source()
    }
}

/// Retirement preserves the complete old parser/plan/cursor owners, the FIFO
/// envelopes and any not-yet-queued event and range request. Its transport has
/// already been aborted/quiesced/closed. These are readable retired facts, never
/// current feed permission. Dropping the receipt releases only its own aliases.
pub struct RetiredCatalogue {
    pub(crate) engine: Option<Engine>,
    pub(crate) queue: BudgetVec<Option<Output>>,
    pub(crate) head: usize,
    pub(crate) pending: Option<RawEvent>,
    pub(crate) pending_source: Option<SourceId>,
    pub(crate) request: Option<InputRequest>,
    pub(crate) seek: Option<SeekIntent>,
}
impl RetiredCatalogue {
    /// Cursor into the retired engine's current owned plan, not a physical I/O
    /// position or an inferred native demuxer cursor.
    pub fn indexed_row_cursor(&self) -> Option<usize> {
        match self.engine.as_ref() {
            Some(Engine::Mp4(engine)) => Some(engine.row),
            _ => None,
        }
    }
    pub fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }
    pub fn queued(&self) -> usize {
        self.queue.len() - self.head
    }
    pub fn pop(&mut self) -> Option<Output> {
        let slot = self.queue.as_mut_slice().get_mut(self.head)?;
        let out = slot.take();
        self.head += 1;
        out
    }
    pub fn pending_event(&self) -> Option<&RawEvent> {
        self.pending.as_ref()
    }
    pub fn pending_source(&self) -> Option<SourceId> {
        self.pending_source
    }
    pub fn pending_request(&self) -> Option<&InputRequest> {
        self.request.as_ref()
    }
    pub fn seek_intent(&self) -> Option<SeekIntent> {
        self.seek
    }
}

/// A planned MP4 range seek, from either clock's seek command.
#[derive(Clone, Copy)]
pub(crate) struct RangeSeek<'a> {
    pub(crate) pool: &'a Budget,
    pub(crate) active_generation: Option<u64>,
    pub(crate) candidate_generation: u64,
    pub(crate) selection: Selection,
    pub(crate) intent: SeekIntent,
    pub(crate) landing: Mp4RangeLanding<'a>,
}

/// Prepared reservations and exclusive borrows make commit allocation-free.
/// Dropping this transaction leaves both player and candidate unchanged.
pub struct CatalogueCommit<'a, S: Source> {
    pub(crate) player: &'a mut Player<S>,
    pub(crate) next: &'a mut Option<Mp4Catalogue<S>>,
    pub(crate) selection: Selection,
    pub(crate) epoch: u64,
    pub(crate) queue: BudgetVec<Option<Output>>,
    pub(crate) cancel: &'a AtomicBool,
    pub(crate) seek: Option<(SeekIntent, Mp4RangeLanding<'a>)>,
}
impl<S: Source> CatalogueCommit<'_, S> {
    pub fn boundary(&self) -> Result<CatalogueBoundary> {
        let engine = self.next_engine()?;
        Ok(CatalogueBoundary {
            source: engine.source(),
            input_generation: engine.generation(),
            delivery_epoch: self.epoch,
        })
    }
    pub fn next_engine(&self) -> Result<&Engine> {
        Ok(&self
            .next
            .as_ref()
            .ok_or(Error::Invalid("replacement missing"))?
            .engine)
    }
    /// Cancellation here cancels only the replacement attempt. Ordinary
    /// Player::advance cancellation still tears down the active player.
    pub fn commit(self) -> Result<RetiredCatalogue> {
        self.commit_with_boundary().map(|(retired, _)| retired)
    }

    pub(crate) fn commit_with_boundary(self) -> Result<(RetiredCatalogue, CatalogueBoundary)> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let next = self
            .next
            .as_mut()
            .ok_or(Error::Invalid("replacement missing"))?;
        let seek = if let Some((intent, landing)) = self.seek {
            next.engine.begin_seek(&intent, Some(landing))?;
            Some(intent)
        } else {
            None
        };
        let boundary = CatalogueBoundary {
            source: next.engine.source(),
            input_generation: next.engine.generation(),
            delivery_epoch: self.epoch,
        };
        // No fallible operation follows the ownership transfer.
        let next = self.next.take().expect("validated replacement");
        let retired =
            self.player
                .commit_mp4_catalogue(next, self.selection, self.epoch, self.queue);
        if let Some(intent) = seek {
            self.player.stage_seek(intent);
        }
        Ok((retired, boundary))
    }
}
