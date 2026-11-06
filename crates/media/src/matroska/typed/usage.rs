//! Exact operation-owned storage, used only after a real allocation rejection.
//! No allocation, reference-count inference, or global used-byte subtraction is
//! needed to distinguish an impossible operation from caller-retained pressure.
use super::configuration::Configuration;
use crate::{Error, Result, incremental::ParseStep, metadata::SharedDocument};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Reservation, WorkBudget};

#[derive(Debug)]
pub(crate) enum Owner {
    /// A uniquely owned mutable allocation or linear reservation, visited once.
    Unique(usize),
    Bytes(Bytes),
    Configuration(Configuration),
    Document(SharedDocument),
    Group(super::side_data::BlockGroupSource),
    TrackExtensions(super::mapping::TrackExtensions),
    States(super::configuration::CodecStates),
}
impl Owner {
    fn bytes(&self) -> Result<usize> {
        match self {
            Self::Unique(bytes) => Ok(*bytes),
            Self::Bytes(bytes) => Ok(bytes.retained_storage_bytes()),
            Self::Configuration(configuration) => configuration.structure_storage_bytes(),
            Self::Document(document) => document.structure_storage_bytes(),
            Self::Group(group) => group.structure_bytes(),
            Self::TrackExtensions(mapping) => mapping.structure_bytes(),
            Self::States(states) => states.structure_bytes(),
        }
    }
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bytes(a), Self::Bytes(b)) => a.shares_storage_with(b),
            (Self::Configuration(a), Self::Configuration(b)) => a.shares_owner_with(b),
            (Self::Document(a), Self::Document(b)) => a.shares_owner_with(b),
            (Self::Group(a), Self::Group(b)) => a.shares_owner_with(b),
            (Self::TrackExtensions(a), Self::TrackExtensions(b)) => a.shares_owner_with(b),
            (Self::States(a), Self::States(b)) => a.shares_owner_with(b),
            _ => false,
        }
    }
}

/// Slots are addressed directly through bounded field lists and fixed-stride
/// arenas. Looking up a slot never scans an input-sized prefix. Empty slots are
/// allowed so an arena's stride does not depend on which fields are present.
pub(crate) trait Storage {
    fn storage_slots(&self) -> Result<usize>;
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>>;
}
impl<T: Storage + ?Sized> Storage for &T {
    fn storage_slots(&self) -> Result<usize> {
        (**self).storage_slots()
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        (**self).storage_owner(index)
    }
}
impl<T: Storage> Storage for Option<T> {
    fn storage_slots(&self) -> Result<usize> {
        self.as_ref().map_or(Ok(0), Storage::storage_slots)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        self.as_ref()
            .map_or(Ok(None), |value| value.storage_owner(index))
    }
}
impl<T> Storage for BudgetVec<T> {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        if index != 0 {
            return Ok(None);
        }
        let bytes = self
            .capacity()
            .checked_mul(size_of::<T>())
            .ok_or(Error::Overflow)?;
        Ok((bytes != 0).then_some(Owner::Unique(bytes)))
    }
}
impl Storage for Reservation {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        Ok((index == 0 && self.bytes() != 0).then(|| Owner::Unique(self.bytes())))
    }
}
impl Storage for Bytes {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        Ok((index == 0).then(|| Owner::Bytes(self.clone())))
    }
}
#[derive(Debug)]
pub(crate) struct Charge(pub usize);
impl Storage for Charge {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        Ok((index == 0 && self.0 != 0).then_some(Owner::Unique(self.0)))
    }
}
pub(crate) fn slots(parts: &[&dyn Storage]) -> Result<usize> {
    parts.iter().try_fold(0_usize, |sum, part| {
        sum.checked_add(part.storage_slots()?)
            .ok_or(Error::Overflow)
    })
}
pub(crate) fn owner(parts: &[&dyn Storage], mut index: usize) -> Result<Option<Owner>> {
    for part in parts {
        let count = part.storage_slots()?;
        if index < count {
            return part.storage_owner(index);
        }
        index -= count;
    }
    Ok(None)
}
// Fixed field lists only. Source-sized sequences use the strided wrappers below.
macro_rules! storage {
    ($ty:ty, |$this:ident| [$($part:expr),* $(,)?]) => {
        impl $crate::matroska::typed::usage::Storage for $ty {
            fn storage_slots(&self) -> $crate::Result<usize> {
                let $this = self;
                $crate::matroska::typed::usage::slots(&[$(&$part as &dyn $crate::matroska::typed::usage::Storage),*])
            }
            fn storage_owner(&self, index: usize) -> $crate::Result<Option<$crate::matroska::typed::usage::Owner>> {
                let $this = self;
                $crate::matroska::typed::usage::owner(&[$(&$part as &dyn $crate::matroska::typed::usage::Storage),*], index)
            }
        }
    };
}
pub(crate) use storage;

pub(crate) struct Tracks<'a>(pub &'a [crate::matroska::Track]);
impl Storage for Tracks<'_> {
    fn storage_slots(&self) -> Result<usize> {
        self.0.len().checked_mul(15).ok_or(Error::Overflow)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        self.0
            .get(index / 15)
            .map_or(Ok(None), |track| track.storage_owner(index % 15))
    }
}
pub(crate) struct Documents<'a>(pub &'a [SharedDocument]);
impl Storage for Documents<'_> {
    fn storage_slots(&self) -> Result<usize> {
        self.0.len().checked_mul(2).ok_or(Error::Overflow)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        self.0
            .get(index / 2)
            .map_or(Ok(None), |document| document.storage_owner(index % 2))
    }
}
pub(crate) struct Packets<'a>(pub &'a [Option<crate::matroska::Packet>]);
impl Storage for Packets<'_> {
    fn storage_slots(&self) -> Result<usize> {
        Ok(self.0.len())
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        self.0
            .get(index)
            .and_then(Option::as_ref)
            .map_or(Ok(None), |packet| packet.data.storage.storage_owner(0))
    }
}

/// Cold, allocation-free pairwise deduplication. Each visit/comparison consumes
/// a work unit and checks cancellation. The graph is held fixed while this job
/// is pending; supplying a new input window explicitly restarts it.
#[derive(Debug)]
pub(crate) struct Pressure {
    pub requested: usize,
    index: usize,
    compare: usize,
    candidate: Option<Owner>,
    total: usize,
}
impl Pressure {
    pub fn new(requested: usize) -> Self {
        Self {
            requested,
            index: 0,
            compare: 0,
            candidate: None,
            total: Budget::control_storage_bytes(),
        }
    }
    pub fn step(
        &mut self,
        graph: &dyn Storage,
        pool: &Budget,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<()>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            // Every accumulated term is an unavoidable owner at the rejected
            // allocation point. An early proof cannot become false after more
            // deduplicated nonnegative terms are visited.
            if self
                .total
                .checked_add(self.requested)
                .is_none_or(|total| total > pool.limit())
            {
                return Err(Error::Limit("operation retained storage"));
            }
            if self.index == graph.storage_slots()? {
                let available = pool.available();
                return Ok(if available >= self.requested {
                    ParseStep::Event(())
                } else {
                    ParseStep::Backpressure {
                        requested: self.requested,
                        available,
                    }
                });
            }
            if !work.try_charge(1) {
                return Ok(ParseStep::Yield);
            }
            if let Some(candidate) = &self.candidate {
                if self.compare == self.index {
                    self.total = self
                        .total
                        .checked_add(candidate.bytes()?)
                        .ok_or(Error::Overflow)?;
                    self.candidate = None;
                    self.index += 1;
                } else {
                    let duplicate = graph
                        .storage_owner(self.compare)?
                        .is_some_and(|previous| candidate.same(&previous));
                    self.compare += 1;
                    if duplicate {
                        self.candidate = None;
                        self.index += 1;
                    }
                }
            } else {
                match graph.storage_owner(self.index)? {
                    Some(Owner::Unique(bytes)) => {
                        self.total = self.total.checked_add(bytes).ok_or(Error::Overflow)?;
                        self.index += 1;
                    }
                    Some(candidate) => {
                        self.candidate = Some(candidate);
                        self.compare = 0;
                    }
                    None => self.index += 1,
                }
            }
        }
    }
}

impl Storage for triergon_core::InputCursor {
    fn storage_slots(&self) -> Result<usize> {
        Ok(1)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        Ok(if index == 0 {
            self.retained_window().map(Owner::Bytes)
        } else {
            None
        })
    }
}
impl Storage for crate::matroska::Track {
    fn storage_slots(&self) -> Result<usize> {
        Ok(15)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        match index {
            0 => self.source_metadata.storage_owner(0),
            1 => self.codec_private.storage_owner(0),
            2 => self.packet_prefix.storage_owner(0),
            3 => self.codec.storage_owner(0),
            4 => self.name.storage_owner(0),
            5 => self.language.storage_owner(0),
            6 => self.language_bcp47.storage_owner(0),
            7..=14 => self
                .packet_decoder
                .as_ref()
                .map_or(Ok(None), |decoder| decoder.storage_owner(index - 7)),
            _ => Ok(None),
        }
    }
}

/// Ephemeral view of an operation and its explicit immutable input owner.
/// Existing Bytes identity deduplication counts windows/packets/root once.
/// No second source handle is retained in Reader and ordinary callers keep
/// their original graph, slots and pressure-work sequence.
pub(crate) struct WithSource<'a> {
    pub graph: &'a dyn Storage,
    pub source: &'a dyn Storage,
}
impl Storage for WithSource<'_> {
    fn storage_slots(&self) -> Result<usize> {
        self.graph
            .storage_slots()?
            .checked_add(self.source.storage_slots()?)
            .ok_or(Error::Overflow)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<Owner>> {
        let slots = self.graph.storage_slots()?;
        if index < slots {
            self.graph.storage_owner(index)
        } else {
            self.source.storage_owner(index - slots)
        }
    }
}

#[cfg(test)]
mod owned_source_pressure;
