//! Opaque configuration publication. Raw Tracks semantic construction is a
//! separate retained job; this seal accepts its charged, validated vector.
use super::syntax::check;
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result, incremental::ParseStep, matroska::Track};
use std::{
    mem::size_of,
    sync::{Arc, atomic::AtomicBool},
};
use triergon_core::{Budget, BudgetVec, Reservation, WorkBudget};
#[derive(Debug)]
struct Data {
    tracks: BudgetVec<Track>,
    pool: Budget,
    _charge: Reservation,
}
/// Opaque charged snapshot. The private Arc cannot be weakened by callers.
///
/// ```compile_fail
/// fn weaken(snapshot: &triergon::typed::Configuration) {
///     let _ = std::sync::Arc::downgrade(snapshot);
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Configuration {
    data: Arc<Data>,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    states: Option<CodecStates>,
}
impl Configuration {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn tracks(&self) -> &[Track] {
        self.data.tracks.as_slice()
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.data.pool.same_pool(pool)
    }
    /// The constructor already validated this shared owner against the source
    /// pool. Borrowing it avoids a redundant budget handle in a packet job.
    #[inline(always)]
    pub(crate) fn budget(&self) -> &Budget {
        &self.data.pool
    }
    // Cache rebinding changes only format bookkeeping; retained older handles
    // keep their own generation and share the same charged allocation.
    pub(crate) fn rebind(&self, generation: u64, revision: u64) -> Self {
        Self {
            data: self.data.clone(),
            generation,
            revision,
            states: self.states.clone(),
        }
    }
    pub(crate) fn initial_rebind(&self, generation: u64, revision: u64) -> Self {
        Self {
            data: self.data.clone(),
            generation,
            revision,
            states: None,
        }
    }
    pub(crate) fn has_codec_states(&self) -> bool {
        self.states.is_some()
    }
    pub fn codec_state(&self, track: usize) -> Option<&CodecState> {
        self.tracks().get(track)?;
        self.states
            .as_ref()?
            .0
            .entries
            .as_slice()
            .get(track)?
            .as_ref()
    }
    /// Initial CodecPrivate uses the existing content-restoration result. A
    /// CodecState view carries its written bytes and explicit origin; this API
    /// does not guess a frame/private content-encoding scope for CodecState.
    pub fn initialization(&self, track: usize) -> Option<Initialization<'_>> {
        let initial = self.tracks().get(track)?;
        Some(match self.codec_state(track) {
            Some(state) => Initialization {
                data: state.data(),
                origin: InitializationOrigin::CodecState(state.position),
                generation: self.generation,
                revision: self.revision,
            },
            None => Initialization {
                data: &initial.codec_private,
                origin: InitializationOrigin::InitialTrackEntry,
                generation: self.generation,
                revision: self.revision,
            },
        })
    }
    pub fn track_extensions_job(
        &self,
        track: usize,
        limits: crate::Limits,
    ) -> Result<super::mapping::TrackExtensionsJob> {
        let source = self
            .tracks()
            .get(track)
            .required("unknown configuration track")?
            .source_metadata
            .clone();
        super::mapping::TrackExtensionsJob::new(source, limits, &self.data.pool)
    }
}
#[derive(Debug)]
pub(crate) struct ConfigurationJob {
    pool: Budget,
    tracks: Option<BudgetVec<Track>>,
    generation: u64,
    revision: u64,
    result: Option<Configuration>,
    failed: bool,
    states: Option<CodecStates>,
}
impl ConfigurationJob {
    /// Internal seam, not a public raw-Track admission API. The future Tracks
    /// builder must create this vector in pool and complete all source validation
    /// before calling it. No untracked Vec is adopted here.
    pub fn from_validated_vec(
        tracks: BudgetVec<Track>,
        pool: &Budget,
        generation: u64,
        revision: u64,
    ) -> Self {
        Self {
            pool: pool.clone(),
            tracks: Some(tracks),
            generation,
            revision,
            result: None,
            failed: false,
            states: None,
        }
    }
    /// Only the Tracks builder calls this after preserving the previous Track
    /// prefix. Sparse state slots for newly appended tracks mean initial state.
    pub(crate) fn inherit_states(mut self, previous: Option<&Configuration>) -> Self {
        self.states = previous.and_then(|configuration| configuration.states.clone());
        self
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        if self.failed {
            return Err(Error::Invalid(
                "configuration seal requires replacement after error",
            ));
        }
        if let Err(error) = check(cancel) {
            self.failed = true;
            return Err(error);
        }
        if let Some(result) = &self.result {
            return Ok(ParseStep::Event(result.clone()));
        }
        if !work.try_charge(1) {
            return Ok(ParseStep::Yield);
        }
        let charge = match self
            .pool
            .reserve(size_of::<Data>() + 2 * size_of::<usize>())
        {
            Ok(charge) => charge,
            Err(triergon_core::Error::Backpressure {
                requested,
                available,
            }) => {
                return Ok(ParseStep::Backpressure {
                    requested,
                    available,
                });
            }
            Err(error) => {
                self.failed = true;
                return Err(error.into());
            }
        };
        let tracks = self.tracks.take().required("missing validated tracks")?;
        let data = Arc::new(Data {
            tracks,
            pool: self.pool.clone(),
            _charge: charge,
        });
        let result = Configuration {
            data,
            generation: self.generation,
            revision: self.revision,
            states: self.states.clone(),
        };
        self.result = Some(result.clone());
        Ok(ParseStep::Event(result))
    }
}

impl Configuration {
    pub(crate) fn shares_owner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
    pub(crate) fn structure_storage_bytes(&self) -> Result<usize> {
        self.data
            .tracks
            .capacity()
            .checked_mul(size_of::<Track>())
            .and_then(|bytes| bytes.checked_add(self.data._charge.bytes()))
            .ok_or(Error::Overflow)
    }
}
impl super::usage::Storage for Configuration {
    fn storage_slots(&self) -> Result<usize> {
        super::usage::slots(&[
            &super::usage::Charge(0),
            &super::usage::Tracks(self.tracks()),
            &self.states,
        ])
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        if index == 0 {
            Ok(Some(super::usage::Owner::Configuration(self.clone())))
        } else {
            super::usage::owner(
                &[&super::usage::Tracks(self.tracks()), &self.states],
                index - 1,
            )
        }
    }
}
super::usage::storage!(ConfigurationJob, |s| [
    s.tracks,
    super::usage::Tracks(s.tracks.as_ref().map_or(&[][..], BudgetVec::as_slice)),
    s.result,
    s.states
]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializationOrigin {
    InitialTrackEntry,
    CodecState(Option<super::side_data::SourcePosition>),
}
#[derive(Clone, Copy, Debug)]
pub struct Initialization<'a> {
    pub data: &'a [u8],
    pub origin: InitializationOrigin,
    pub generation: u64,
    pub revision: u64,
}
/// Raw written CodecState bytes. This does not infer a content-encoding scope or
/// a decoder/native packet-side-data layout from the initial CodecPrivate.
#[derive(Clone, Debug)]
pub struct CodecState {
    data: crate::storage::SharedBytes,
    position: Option<super::side_data::SourcePosition>,
}
impl CodecState {
    pub fn data(&self) -> &[u8] {
        &self.data
    }
    pub fn position(&self) -> Option<super::side_data::SourcePosition> {
        self.position
    }
    pub fn retained_capacity(&self) -> usize {
        self.data.retained_capacity()
    }
}
#[derive(Debug)]
struct StateData {
    entries: BudgetVec<Option<CodecState>>,
    _charge: Reservation,
}
#[derive(Clone, Debug)]
pub(crate) struct CodecStates(Arc<StateData>);
impl CodecStates {
    pub(crate) fn shares_owner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn structure_bytes(&self) -> Result<usize> {
        self.0
            .entries
            .capacity()
            .checked_mul(size_of::<Option<CodecState>>())
            .and_then(|n| n.checked_add(self.0._charge.bytes()))
            .ok_or(Error::Overflow)
    }
}
#[derive(Debug)]
pub(crate) struct StateCopyJob {
    copy: crate::zlib::typed_restore::CopyJob,
    position: Option<super::side_data::SourcePosition>,
    result: Option<CodecState>,
}
impl StateCopyJob {
    pub(crate) fn new(
        group: &super::side_data::BlockGroupSource,
        limit: usize,
        pool: &Budget,
    ) -> Result<Self> {
        let state = group.codec_state().required("missing CodecState source")?;
        Ok(Self {
            copy: crate::zlib::typed_restore::CopyJob::new(
                crate::storage::SharedBytes::default(),
                state.owner()?,
                limit,
                pool,
            )?,
            position: state.position()?,
            result: None,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<CodecState>> {
        check(cancel)?;
        if let Some(result) = &self.result {
            return Ok(ParseStep::Event(result.clone()));
        }
        match self.copy.step(work, cancel)? {
            ParseStep::Event(data) => {
                let state = CodecState {
                    data,
                    position: self.position,
                };
                self.result = Some(state.clone());
                Ok(ParseStep::Event(state))
            }
            other => super::pending(other),
        }
    }
}
#[derive(Debug)]
pub(crate) struct StateChangeJob {
    base: Configuration,
    pool: Budget,
    track: usize,
    state: Option<CodecState>,
    entries: Option<BudgetVec<Option<CodecState>>>,
    at: usize,
    reserved: bool,
    result: Option<Configuration>,
    failed: bool,
}
impl StateChangeJob {
    pub(crate) fn new(
        base: Configuration,
        track: usize,
        state: Option<CodecState>,
        pool: &Budget,
    ) -> Result<Self> {
        if !base.belongs_to(pool)
            || state
                .as_ref()
                .is_some_and(|state| !state.data.belongs_to(pool))
        {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if track >= base.tracks().len() {
            return Err(Error::Invalid("unknown state track"));
        }
        base.revision.checked_add(1).ok_or(Error::Overflow)?;
        Ok(Self {
            base,
            pool: pool.clone(),
            track,
            state,
            entries: Some(BudgetVec::new(pool)),
            at: 0,
            reserved: false,
            result: None,
            failed: false,
        })
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        if self.failed {
            return Err(Error::Invalid(
                "state change requires replacement after error",
            ));
        }
        match self.advance(work, cancel) {
            Err(Error::Backpressure {
                requested,
                available,
            }) => Ok(ParseStep::Backpressure {
                requested,
                available,
            }),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
            other => other,
        }
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Configuration>> {
        loop {
            check(cancel)?;
            if let Some(result) = &self.result {
                return Ok(ParseStep::Event(result.clone()));
            }
            if !self.reserved {
                if !work.try_charge(1) {
                    return Ok(ParseStep::Yield);
                }
                self.entries
                    .as_mut()
                    .required("missing state arena")?
                    .try_reserve_exact(self.base.tracks().len())?;
                self.reserved = true;
            }
            if self.at != self.base.tracks().len() {
                if !work.try_charge(1) {
                    return Ok(ParseStep::Yield);
                }
                let state = if self.at == self.track {
                    self.state.clone()
                } else {
                    self.base.codec_state(self.at).cloned()
                };
                self.entries
                    .as_mut()
                    .required("missing state arena")?
                    .try_push(state)
                    .map_err(|error| Error::from(error.error))?;
                self.at += 1;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            let charge = self
                .pool
                .reserve(size_of::<StateData>() + 2 * size_of::<usize>())?;
            let entries = self.entries.take().required("missing state arena")?;
            let states = CodecStates(Arc::new(StateData {
                entries,
                _charge: charge,
            }));
            let result = Configuration {
                data: self.base.data.clone(),
                generation: self.base.generation,
                revision: self.base.revision.checked_add(1).ok_or(Error::Overflow)?,
                states: Some(states),
            };
            self.result = Some(result.clone());
            return Ok(ParseStep::Event(result));
        }
    }
}
impl super::usage::Storage for CodecState {
    fn storage_slots(&self) -> Result<usize> {
        super::usage::Storage::storage_slots(&self.data)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        super::usage::Storage::storage_owner(&self.data, index)
    }
}
impl super::usage::Storage for CodecStates {
    fn storage_slots(&self) -> Result<usize> {
        self.0.entries.len().checked_add(1).ok_or(Error::Overflow)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        if index == 0 {
            return Ok(Some(super::usage::Owner::States(self.clone())));
        }
        self.0
            .entries
            .as_slice()
            .get(index - 1)
            .and_then(Option::as_ref)
            .map_or(Ok(None), |state| {
                super::usage::Storage::storage_owner(state, 0)
            })
    }
}
struct StateEntries<'a>(&'a [Option<CodecState>]);
impl super::usage::Storage for StateEntries<'_> {
    fn storage_slots(&self) -> Result<usize> {
        Ok(self.0.len())
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        self.0
            .get(index)
            .and_then(Option::as_ref)
            .map_or(Ok(None), |state| {
                super::usage::Storage::storage_owner(state, 0)
            })
    }
}
super::usage::storage!(StateCopyJob, |s| [s.copy, s.result]);
super::usage::storage!(StateChangeJob, |s| [
    s.base,
    s.state,
    s.entries,
    StateEntries(s.entries.as_ref().map_or(&[][..], BudgetVec::as_slice)),
    s.result
]);
