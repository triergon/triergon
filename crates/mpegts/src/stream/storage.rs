//! The storage an open transport stream holds: walked for a refused PES range or span
//! replacement, or whole for the reader's audit inside the stream.

use super::event::TraceEvent;
use super::tables::PsiWork;
use super::{Held, Route, Stage, TransportStream};
use crate::pes::PesAssembler;
use crate::table_storage;
use triergon_core::{Bytes, Error, Result};

pub(crate) enum StorageItem<'a> {
    Fixed(usize),
    Bytes(&'a Bytes),
    // These allocations are already included in the blocked assembler's floor.
    CountedBytes(&'a Bytes),
}

impl<'a> StorageItem<'a> {
    pub(crate) fn backing(self) -> Option<&'a Bytes> {
        match self {
            Self::Bytes(bytes) | Self::CountedBytes(bytes) => Some(bytes),
            Self::Fixed(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportStorageCursor {
    BlockedSpan(usize),
    BlockedPayload,
    PsiFault,
    Carrier,
    PmtSource,
    PatVersion(usize),
    PatSection(usize, usize),
    PatBacking(usize, usize),
    Route(usize),
    Assembler(usize),
    Span(usize, usize),
    Payload(usize),
    End,
}

impl TransportStorageCursor {
    /// The start of a walk of every backing the open stream holds, for a stream that is
    /// not stopped at a refused PES growth.
    pub(crate) const OPEN: Self = Self::PsiFault;

    // Each advance visits at most one section, route, or backing. `blocked` is the route
    // whose refused growth the walk accounts for (its tables are in the growth floor); at
    // that stage PSI jobs have drained and the framer owns only its carry.
    pub(crate) fn step<'a>(
        &mut self,
        stream: &'a TransportStream,
        blocked: Option<usize>,
    ) -> Result<Option<StorageItem<'a>>> {
        let routes = stream.routes.as_slice();
        let blocked_assembler = || -> Result<Option<&'a PesAssembler>> {
            let index = blocked.ok_or(Error::Invalid("PES storage audit outside drain"))?;
            Ok(routes[index].assembler.as_ref())
        };
        let item = match *self {
            Self::BlockedSpan(index) => {
                if let Some(bytes) =
                    blocked_assembler()?.and_then(|assembler| assembler.span_backing(index))
                {
                    *self = Self::BlockedSpan(index + 1);
                    Some(StorageItem::CountedBytes(bytes))
                } else {
                    *self = Self::BlockedPayload;
                    None
                }
            }
            Self::BlockedPayload => {
                *self = Self::Carrier;
                blocked_assembler()?
                    .and_then(PesAssembler::pending_backing)
                    .map(StorageItem::CountedBytes)
            }
            Self::PsiFault => {
                *self = Self::Carrier;
                match &stream.psi {
                    PsiWork::Fault(fault) => fault
                        .rejected
                        .as_ref()
                        .map(|rejected| StorageItem::Bytes(rejected.backing_bytes())),
                    _ => None,
                }
            }
            Self::Carrier => {
                *self = Self::PmtSource;
                stream
                    .held
                    .as_ref()
                    .map(|held| StorageItem::Bytes(&held.packet.source))
            }
            Self::PmtSource => {
                *self = Self::PatVersion(0);
                stream.pmt_source.as_ref().map(StorageItem::Bytes)
            }
            Self::PatVersion(version) => {
                let sections = stream.collector.retained_sections();
                match sections.get(version) {
                    Some(Some(table)) => {
                        *self = Self::PatSection(version, 0);
                        Some(StorageItem::Fixed(table_storage(table)?))
                    }
                    Some(None) => {
                        *self = Self::PatVersion(version + 1);
                        None
                    }
                    None => {
                        *self = Self::Route(0);
                        None
                    }
                }
            }
            Self::PatSection(version, index) => {
                let table = stream.collector.retained_sections()[version]
                    .ok_or(Error::Invalid("PAT storage audit version"))?;
                if let Some(section) = table.as_slice().get(index) {
                    *self = Self::PatBacking(version, index);
                    let bytes = table_storage(&section.programs)?
                        .checked_add(table_storage(&section.source_ranges)?)
                        .ok_or(Error::Overflow)?;
                    Some(StorageItem::Fixed(bytes))
                } else {
                    *self = Self::PatVersion(version + 1);
                    None
                }
            }
            Self::PatBacking(version, index) => {
                *self = Self::PatSection(version, index + 1);
                let table = stream.collector.retained_sections()[version]
                    .ok_or(Error::Invalid("PAT storage audit version"))?;
                Some(StorageItem::Bytes(&table.as_slice()[index].source))
            }
            Self::Route(index) => {
                if let Some(route) = routes.get(index) {
                    *self = Self::Assembler(index);
                    Some(StorageItem::Bytes(&route.descriptors))
                } else {
                    *self = Self::End;
                    None
                }
            }
            Self::Assembler(index) => {
                if Some(index) != blocked
                    && let Some(assembler) = &routes[index].assembler
                {
                    *self = Self::Span(index, 0);
                    Some(StorageItem::Fixed(assembler.table_storage()?))
                } else {
                    *self = Self::Route(index + 1);
                    None
                }
            }
            Self::Span(route, index) => {
                let assembler = routes[route]
                    .assembler
                    .as_ref()
                    .ok_or(Error::Invalid("PES storage audit route"))?;
                if let Some(bytes) = assembler.span_backing(index) {
                    *self = Self::Span(route, index + 1);
                    Some(StorageItem::Bytes(bytes))
                } else {
                    *self = Self::Payload(route);
                    None
                }
            }
            Self::Payload(route) => {
                *self = Self::Route(route + 1);
                routes[route]
                    .assembler
                    .as_ref()
                    .and_then(PesAssembler::pending_backing)
                    .map(StorageItem::Bytes)
            }
            Self::End => None,
        };
        Ok(item)
    }
}

impl TransportStream {
    /// The PES route whose assembly holds the carrier in hand, as it does while a range
    /// or span growth is refused.
    pub(crate) fn assembling_route(&self) -> Option<usize> {
        match (self.route, &self.held) {
            (
                Route::Pes(index),
                Some(Held {
                    stage: Stage::Assemble,
                    ..
                }),
            ) => Some(index),
            _ => None,
        }
    }

    pub(crate) fn blocked_pes_growth_floor(&self) -> Result<Option<usize>> {
        let Some(index) = self.assembling_route() else {
            return Ok(None);
        };
        let Some(span_floor) = self.routes.as_slice()[index]
            .assembler
            .as_ref()
            .and_then(PesAssembler::blocked_growth_floor)
        else {
            return Ok(None);
        };
        span_floor
            .checked_add(self.table_storage()?)
            .map(Some)
            .ok_or(Error::Overflow)
    }

    /// Storage of the open stream that a walk from [`TransportStorageCursor::OPEN`] does
    /// not visit: the route table, the section assemblers' buffers and the framer carry.
    /// `None` while table work holds a section, table or map in progress, which the walk
    /// does not count; the stream holds none of those while it reports an event.
    pub(crate) fn open_storage(&self) -> Result<Option<usize>> {
        if !matches!(self.psi, PsiWork::Idle | PsiWork::Fault(_)) {
            return Ok(None);
        }
        self.table_storage().map(Some)
    }

    fn table_storage(&self) -> Result<usize> {
        let pmt = self
            .pmt
            .as_ref()
            .map_or(Ok(0), |route| route.assembler.table_storage())?;
        table_storage(&self.routes)?
            .checked_add(self.pat.assembler.table_storage()?)
            .and_then(|n| n.checked_add(pmt))
            .and_then(|n| n.checked_add(self.framer.carry_storage().ok()?))
            .ok_or(Error::Overflow)
    }

    pub(crate) fn refuse_blocked_pes_growth(&mut self, error: Error) -> TraceEvent {
        debug_assert!(matches!(self.blocked_pes_growth_floor(), Ok(Some(_))));
        self.discard_faulted_route(error)
    }
}
