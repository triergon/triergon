//! Source-preserving TrackEntry extension parsing, RFC9559 §5.1.4.1.16–17.
//! Registered codec meanings are deliberately separate from these wire facts.
use super::{
    copy_vec::CopyVec,
    side_data::{Presence, Span},
    syntax::{ChildCursor, Required, check},
};
use crate::{
    Error, Limits, Result, ebml,
    incremental::ParseStep,
    storage::{SharedBytes, text_job::TextJob},
};
use std::{
    mem::size_of,
    sync::{Arc, atomic::AtomicBool},
};
use triergon_core::{Budget, BudgetVec, Reservation, WorkBudget};

#[derive(Clone, Copy, Debug)]
struct Mapping {
    span: Span,
    value: Option<(Span, u64)>,
    kind: Option<(Span, u64)>,
    name: Option<Span>,
    name_end: usize,
    extra: Option<Span>,
}
#[derive(Clone, Copy, Debug)]
struct Unknown {
    mapping: usize,
    id: u64,
    span: Span,
}
#[derive(Debug)]
struct Data {
    raw: SharedBytes,
    maximum: Option<(Span, u64)>,
    mappings: BudgetVec<Mapping>,
    unknown: BudgetVec<Unknown>,
    _charge: Reservation,
}
/// Opaque immutable source owner; no public weak handle can escape its charge.
#[derive(Clone, Debug)]
pub struct TrackExtensions(Arc<Data>);
impl TrackExtensions {
    pub fn raw_track_entry(&self) -> &[u8] {
        &self.0.raw
    }
    pub fn max_block_addition_id(&self) -> u64 {
        self.0.maximum.map_or(0, |(_, value)| value)
    }
    pub fn maximum_presence(&self) -> Presence {
        presence(self.0.maximum.map(|(span, _)| span))
    }
    pub fn mappings(&self) -> impl ExactSizeIterator<Item = MappingView<'_>> {
        self.0.mappings.as_slice().iter().map(|record| MappingView {
            owner: self,
            record,
        })
    }
    pub fn unknown_mapping_fields(&self) -> impl ExactSizeIterator<Item = MappingExtension<'_>> {
        self.0
            .unknown
            .as_slice()
            .iter()
            .map(|record| MappingExtension {
                owner: self,
                record,
            })
    }
    pub(crate) fn shares_owner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn structure_bytes(&self) -> Result<usize> {
        self.0
            .mappings
            .capacity()
            .checked_mul(size_of::<Mapping>())
            .and_then(|n| {
                n.checked_add(
                    self.0
                        .unknown
                        .capacity()
                        .checked_mul(size_of::<Unknown>())?,
                )
            })
            .and_then(|n| n.checked_add(self.0._charge.bytes()))
            .ok_or(Error::Overflow)
    }
}
fn presence(span: Option<Span>) -> Presence {
    match span {
        None => Presence::Absent,
        Some(span) if span.data == span.end => Presence::Empty,
        Some(_) => Presence::Value,
    }
}
#[derive(Clone, Copy, Debug)]
pub struct MappingView<'a> {
    owner: &'a TrackExtensions,
    record: &'a Mapping,
}
impl<'a> MappingView<'a> {
    pub fn value(self) -> Option<u64> {
        self.record.value.map(|(_, value)| value)
    }
    pub fn value_presence(self) -> Presence {
        presence(self.record.value.map(|(span, _)| span))
    }
    pub fn kind(self) -> u64 {
        self.record.kind.map_or(0, |(_, value)| value)
    }
    pub fn kind_presence(self) -> Presence {
        presence(self.record.kind.map(|(span, _)| span))
    }
    /// This is a wire conflict marker, not a hidden choice between RFC9559's
    /// type-zero usage note and the newer codec mapping registry/draft.
    pub fn type_zero_value_conflict(self) -> bool {
        self.kind() == 0 && self.value().is_some()
    }
    pub fn name(self) -> Result<Option<&'a str>> {
        self.record
            .name
            .map(|span| {
                std::str::from_utf8(&self.owner.0.raw[span.data..self.record.name_end])
                    .map_err(|_| Error::Invalid("ASCII mapping name"))
            })
            .transpose()
    }
    pub fn raw_name(self) -> Option<&'a [u8]> {
        self.record
            .name
            .map(|span| &self.owner.0.raw[span.payload()])
    }
    pub fn extra_data(self) -> Option<&'a [u8]> {
        self.record
            .extra
            .map(|span| &self.owner.0.raw[span.payload()])
    }
    pub fn raw(self) -> &'a [u8] {
        &self.owner.0.raw[self.record.span.header..self.record.span.end]
    }
}
#[derive(Clone, Copy, Debug)]
pub struct MappingExtension<'a> {
    owner: &'a TrackExtensions,
    record: &'a Unknown,
}
impl<'a> MappingExtension<'a> {
    pub fn mapping_index(self) -> usize {
        self.record.mapping
    }
    pub fn id(self) -> u64 {
        self.record.id
    }
    pub fn raw(self) -> &'a [u8] {
        &self.owner.0.raw[self.record.span.header..self.record.span.end]
    }
    pub fn data(self) -> &'a [u8] {
        &self.owner.0.raw[self.record.span.payload()]
    }
}
#[derive(Debug)]
enum Phase {
    Root,
    Mapping,
    Name,
    Store,
    Unknown,
    Seal,
    Done,
}
/// Real retained cursors and bounded name validation. This job reads only the
/// existing charged TrackEntry bytes and performs no transport or time work.
#[derive(Debug)]
pub struct TrackExtensionsJob {
    raw: SharedBytes,
    pool: Budget,
    limit: usize,
    root: ChildCursor,
    cursor: Option<ChildCursor>,
    maximum: Option<(Span, u64)>,
    record: Option<Mapping>,
    mappings: CopyVec<Mapping>,
    unknown: CopyVec<Unknown>,
    pending: Option<Unknown>,
    queued: bool,
    text: Option<TextJob>,
    phase: Phase,
    result: Option<TrackExtensions>,
    pressure: Option<super::usage::Pressure>,
    failed: bool,
}
impl TrackExtensionsJob {
    pub(crate) fn new(raw: SharedBytes, limits: Limits, pool: &Budget) -> Result<Self> {
        if !raw.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool);
        }
        if raw.len() > limits.metadata_bytes {
            return Err(Error::Policy("track extension bytes"));
        }
        let length = raw.len();
        Ok(Self {
            raw,
            pool: pool.clone(),
            limit: limits.metadata_nodes,
            root: ChildCursor::new(0..length),
            cursor: None,
            maximum: None,
            record: None,
            mappings: CopyVec::new(pool),
            unknown: CopyVec::new(pool),
            pending: None,
            queued: false,
            text: None,
            phase: Phase::Root,
            result: None,
            pressure: None,
            failed: false,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<TrackExtensions>> {
        if self.failed {
            return Err(Error::Invalid(
                "track extension job requires replacement after error",
            ));
        }
        if let Some(mut pressure) = self.pressure.take() {
            match pressure.step(self, &self.pool, work, cancel) {
                Ok(ParseStep::Event(())) => return Ok(ParseStep::Yield),
                Ok(other) => {
                    self.pressure = Some(pressure);
                    return super::pending(other);
                }
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        match self.advance(work, cancel) {
            Err(Error::Backpressure {
                requested,
                available,
            }) => {
                self.pressure = Some(super::usage::Pressure::new(requested));
                Ok(ParseStep::Backpressure {
                    requested,
                    available,
                })
            }
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
    ) -> Result<ParseStep<TrackExtensions>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Root => match self.root.step(&self.raw, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => self.phase = Phase::Seal,
                    ParseStep::Event(Some(child)) => {
                        let span = Span {
                            header: child.header,
                            data: child.payload.start,
                            end: child.payload.end,
                        };
                        match child.id {
                            0x55ee => {
                                if self.maximum.is_some() {
                                    return Err(Error::Invalid("duplicate MaxBlockAdditionID"));
                                }
                                self.maximum = Some((span, ebml::uint(&self.raw[span.payload()])?));
                            }
                            0x41e4 => {
                                if self.mappings.len() >= self.limit {
                                    return Err(Error::Policy("track mapping descriptors"));
                                }
                                self.record = Some(Mapping {
                                    span,
                                    value: None,
                                    kind: None,
                                    name: None,
                                    name_end: 0,
                                    extra: None,
                                });
                                self.cursor = Some(ChildCursor::new(span.payload()));
                                self.phase = Phase::Mapping;
                            }
                            _ => (), // Complete original TrackEntry remains in the owner.
                        }
                    }
                    _ => return Err(Error::Invalid("track extension root transition")),
                },
                Phase::Mapping => match self
                    .cursor
                    .as_mut()
                    .required("missing mapping cursor")?
                    .step(&self.raw, work, cancel)?
                {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        self.queued = false;
                        self.phase = Phase::Store;
                    }
                    ParseStep::Event(Some(child)) => {
                        let span = Span {
                            header: child.header,
                            data: child.payload.start,
                            end: child.payload.end,
                        };
                        let record = self.record.as_mut().required("missing mapping record")?;
                        match child.id {
                            0x41f0 => {
                                if record.value.is_some() {
                                    return Err(Error::Invalid("duplicate BlockAddIDValue"));
                                }
                                let value = ebml::uint(&self.raw[span.payload()])?;
                                if value < 2 {
                                    return Err(Error::Invalid("BlockAddIDValue below two"));
                                }
                                record.value = Some((span, value));
                            }
                            0x41e7 => {
                                if record.kind.is_some() {
                                    return Err(Error::Invalid("duplicate BlockAddIDType"));
                                }
                                record.kind = Some((span, ebml::uint(&self.raw[span.payload()])?));
                            }
                            0x41a4 => {
                                if record.name.replace(span).is_some() {
                                    return Err(Error::Invalid("duplicate BlockAddIDName"));
                                }
                                self.text =
                                    Some(TextJob::new(self.raw.clone(), span.payload(), true)?);
                                self.phase = Phase::Name;
                            }
                            0x41ed => {
                                if record.extra.replace(span).is_some() {
                                    return Err(Error::Invalid("duplicate BlockAddIDExtraData"));
                                }
                            }
                            _ => {
                                let count = self
                                    .mappings
                                    .len()
                                    .checked_add(self.unknown.len())
                                    .ok_or(Error::Overflow)?;
                                if count >= self.limit {
                                    return Err(Error::Policy("track mapping descriptors"));
                                }
                                self.pending = Some(Unknown {
                                    mapping: self.mappings.len(),
                                    id: child.id,
                                    span,
                                });
                                self.queued = false;
                                self.phase = Phase::Unknown;
                            }
                        }
                    }
                    _ => return Err(Error::Invalid("track mapping transition")),
                },
                Phase::Name => match self
                    .text
                    .as_mut()
                    .required("missing mapping name job")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(value) => {
                        self.record
                            .as_mut()
                            .required("missing mapping record")?
                            .name_end = value.range.end;
                        self.text = None;
                        self.phase = Phase::Mapping;
                    }
                    other => return super::pending(other),
                },
                Phase::Store => {
                    if !self.queued {
                        self.mappings
                            .push(self.record.required("missing mapping record")?)?;
                        self.queued = true;
                    }
                    match self.mappings.step(work, cancel)? {
                        ParseStep::Event(()) => {
                            self.record = None;
                            self.cursor = None;
                            self.queued = false;
                            self.phase = Phase::Root;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Unknown => {
                    if !self.queued {
                        self.unknown
                            .push(self.pending.required("missing mapping extension")?)?;
                        self.queued = true;
                    }
                    match self.unknown.step(work, cancel)? {
                        ParseStep::Event(()) => {
                            self.pending = None;
                            self.queued = false;
                            self.phase = Phase::Mapping;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Seal => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let charge = self
                        .pool
                        .reserve(size_of::<Data>() + 2 * size_of::<usize>())?;
                    let mappings =
                        std::mem::replace(&mut self.mappings, CopyVec::new(&self.pool)).finish()?;
                    let unknown =
                        std::mem::replace(&mut self.unknown, CopyVec::new(&self.pool)).finish()?;
                    self.result = Some(TrackExtensions(Arc::new(Data {
                        raw: self.raw.clone(),
                        maximum: self.maximum,
                        mappings,
                        unknown,
                        _charge: charge,
                    })));
                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing track extensions")?
                            .clone(),
                    ));
                }
            }
        }
    }
}
impl super::usage::Storage for TrackExtensions {
    fn storage_slots(&self) -> Result<usize> {
        Ok(2)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        match index {
            0 => Ok(Some(super::usage::Owner::TrackExtensions(self.clone()))),
            1 => super::usage::Storage::storage_owner(&self.0.raw, 0),
            _ => Ok(None),
        }
    }
}
super::usage::storage!(TrackExtensionsJob, |s| [
    s.raw, s.mappings, s.unknown, s.text, s.result
]);
