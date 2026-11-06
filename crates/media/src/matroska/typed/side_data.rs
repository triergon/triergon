//! RFC9559 §§5.1.3.5.1–6, 16; RFC8794 §§6.1,7.2,11.3.1.
//! Raw group metadata, never an inferred codec/native side-data interpretation.
use super::{
    copy_vec::CopyVec,
    syntax::{Child, ChildCursor, Required, check},
};
use crate::{Error, Limits, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::{
    mem::size_of,
    ops::Range,
    sync::{Arc, atomic::AtomicBool},
};
use triergon_core::{Budget, BudgetVec, Reservation, SourceId, WorkBudget};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    pub source: SourceId,
    pub header: u64,
    pub data: u64,
    pub length: u64,
    pub segment_position: u64,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupLocation {
    pub source: SourceId,
    pub segment_data: u64,
    pub header: u64,
    pub data: u64,
}
impl GroupLocation {
    fn validate(self, length: usize) -> Result<()> {
        if self.header < self.segment_data || self.data < self.header {
            return Err(Error::Invalid("BlockGroup source location"));
        }
        self.data
            .checked_add(length as u64)
            .ok_or(Error::Overflow)?;
        Ok(())
    }
    fn position(self, span: Span) -> Result<SourcePosition> {
        let header = self
            .data
            .checked_add(span.header as u64)
            .ok_or(Error::Overflow)?;
        Ok(SourcePosition {
            source: self.source,
            header,
            data: self
                .data
                .checked_add(span.data as u64)
                .ok_or(Error::Overflow)?,
            length: (span.end - span.data) as u64,
            segment_position: header
                .checked_sub(self.segment_data)
                .ok_or(Error::Overflow)?,
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Span {
    pub header: usize,
    pub data: usize,
    pub end: usize,
}
impl Span {
    fn from_child(child: &Child) -> Self {
        Self {
            header: child.header,
            data: child.payload.start,
            end: child.payload.end,
        }
    }
    pub(crate) fn payload(self) -> Range<usize> {
        self.data..self.end
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Absent,
    Empty,
    Value,
}
#[derive(Clone, Copy, Debug)]
struct Addition {
    id: u64,
    id_field: Option<Span>,
    more: Span,
    data: Span,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionScope {
    Group,
    Additions,
    More(usize),
}
#[derive(Clone, Copy, Debug)]
struct Extension {
    id: u64,
    scope: ExtensionScope,
    span: Span,
}

#[derive(Debug)]
struct Data {
    raw: SharedBytes,
    additions: BudgetVec<Addition>,
    extensions: BudgetVec<Extension>,
    references: BudgetVec<i64>,
    block: Span,
    duration: Option<(Span, u64)>,
    discard: Option<(Span, i64)>,
    state: Option<Span>,
    highest: u64,
    location: Option<GroupLocation>,
    pool: Budget,
    charge: Reservation,
}
/// An opaque owner: clones share the complete charged source and descriptor arenas.
///
/// ```compile_fail
/// fn weaken(value: &triergon_matroska::typed::BlockGroupSource) {
///     let _ = std::sync::Arc::downgrade(value);
/// }
/// ```
#[derive(Clone, Debug)]
pub struct BlockGroupSource(Arc<Data>);
impl BlockGroupSource {
    pub fn raw_body(&self) -> &[u8] {
        &self.0.raw
    }
    pub fn belongs_to(&self, pool: &Budget) -> bool {
        self.0.pool.same_pool(pool)
    }
    pub fn additions(&self) -> impl ExactSizeIterator<Item = AdditionView<'_>> {
        self.0
            .additions
            .as_slice()
            .iter()
            .map(|record| AdditionView {
                group: self,
                record,
            })
    }
    pub fn extensions(&self) -> impl ExactSizeIterator<Item = ExtensionView<'_>> {
        self.0
            .extensions
            .as_slice()
            .iter()
            .map(|record| ExtensionView {
                group: self,
                record,
            })
    }
    pub fn references(&self) -> &[i64] {
        self.0.references.as_slice()
    }
    pub fn duration_ticks(&self) -> Option<u64> {
        self.0.duration.map(|(_, value)| value)
    }
    pub fn discard_padding_ns(&self) -> Option<i64> {
        self.0.discard.map(|(_, value)| value)
    }
    pub fn codec_state(&self) -> Option<StateView<'_>> {
        self.0.state.map(|span| StateView { group: self, span })
    }
    pub fn shares_owner_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn max_addition_id(&self) -> u64 {
        self.0.highest
    }
    pub fn position(&self) -> Result<Option<SourcePosition>> {
        self.0
            .location
            .map(|location| {
                Ok(SourcePosition {
                    source: location.source,
                    header: location.header,
                    data: location.data,
                    length: self.0.raw.len() as u64,
                    segment_position: location
                        .header
                        .checked_sub(location.segment_data)
                        .ok_or(Error::Overflow)?,
                })
            })
            .transpose()
    }
    pub(crate) fn block(&self) -> Range<usize> {
        self.0.block.payload()
    }
    pub(crate) fn structure_bytes(&self) -> Result<usize> {
        self.0
            .additions
            .capacity()
            .checked_mul(size_of::<Addition>())
            .and_then(|n| {
                n.checked_add(
                    self.0
                        .extensions
                        .capacity()
                        .checked_mul(size_of::<Extension>())?,
                )
            })
            .and_then(|n| {
                n.checked_add(self.0.references.capacity().checked_mul(size_of::<i64>())?)
            })
            .and_then(|n| n.checked_add(self.0.charge.bytes()))
            .ok_or(Error::Overflow)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct AdditionView<'a> {
    group: &'a BlockGroupSource,
    record: &'a Addition,
}
impl<'a> AdditionView<'a> {
    pub fn id(self) -> u64 {
        self.record.id
    }
    pub fn id_presence(self) -> Presence {
        match self.record.id_field {
            None => Presence::Absent,
            Some(span) if span.data == span.end => Presence::Empty,
            Some(_) => Presence::Value,
        }
    }
    pub fn data(self) -> &'a [u8] {
        &self.group.0.raw[self.record.data.payload()]
    }
    pub fn raw_more(self) -> &'a [u8] {
        &self.group.0.raw[self.record.more.header..self.record.more.end]
    }
    pub fn position(self) -> Result<Option<SourcePosition>> {
        self.group
            .0
            .location
            .map(|location| location.position(self.record.data))
            .transpose()
    }
}
#[derive(Clone, Copy, Debug)]
pub struct ExtensionView<'a> {
    group: &'a BlockGroupSource,
    record: &'a Extension,
}
impl<'a> ExtensionView<'a> {
    pub fn id(self) -> u64 {
        self.record.id
    }
    pub fn scope(self) -> ExtensionScope {
        self.record.scope
    }
    pub fn raw(self) -> &'a [u8] {
        &self.group.0.raw[self.record.span.header..self.record.span.end]
    }
    pub fn data(self) -> &'a [u8] {
        &self.group.0.raw[self.record.span.payload()]
    }
}
#[derive(Clone, Copy, Debug)]
pub struct StateView<'a> {
    group: &'a BlockGroupSource,
    span: Span,
}
impl<'a> StateView<'a> {
    pub fn data(self) -> &'a [u8] {
        &self.group.0.raw[self.span.payload()]
    }
    pub fn raw(self) -> &'a [u8] {
        &self.group.0.raw[self.span.header..self.span.end]
    }
    pub fn position(self) -> Result<Option<SourcePosition>> {
        self.group
            .0
            .location
            .map(|location| location.position(self.span))
            .transpose()
    }
    pub(crate) fn owner(self) -> Result<SharedBytes> {
        self.group.0.raw.slice(self.span.payload())
    }
}

#[derive(Clone, Copy, Debug)]
enum Scope {
    Group,
    Additions,
    More(usize),
}
#[derive(Debug)]
struct Frame {
    scope: Scope,
    span: Span,
    cursor: ChildCursor,
    crc: Option<(Span, u32)>,
    checked: bool,
}
#[derive(Clone, Copy, Debug)]
struct More {
    span: Span,
    id: u64,
    id_field: Option<Span>,
    data: Option<Span>,
}
#[derive(Clone, Copy, Debug)]
enum Pending {
    Addition(Addition),
    Extension(Extension),
    Reference(i64),
}
#[derive(Debug)]
enum Phase {
    Parse,
    Flush,
    Crc,
    IdReserve,
    IdFill,
    IdSort,
    IdCheck,
    Seal,
    Done,
}
#[derive(Debug)]
pub(crate) struct GroupJob {
    raw: SharedBytes,
    pool: Budget,
    limit: usize,
    maximum: u64,
    highest: u64,
    location: Option<GroupLocation>,
    frames: [Option<Frame>; 3],
    depth: usize,
    more: Option<More>,
    block: Option<Span>,
    duration: Option<(Span, u64)>,
    discard: Option<(Span, i64)>,
    state: Option<Span>,
    additions_seen: bool,
    zero_reference: bool,
    additions: CopyVec<Addition>,
    extensions: CopyVec<Extension>,
    references: CopyVec<i64>,
    pending: Option<Pending>,
    queued: bool,
    ids: BudgetVec<u64>,
    heap: Option<crate::metadata::typed_document::Heap>,
    at: usize,
    crc: u32,
    phase: Phase,
    result: Option<BlockGroupSource>,
    failed: bool,
}
impl GroupJob {
    pub(crate) fn new(
        raw: SharedBytes,
        maximum: u64,
        location: Option<GroupLocation>,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        if !raw.belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if raw.len() > limits.block_bytes {
            return Err(Error::Policy("element bytes"));
        }
        if let Some(location) = location {
            location.validate(raw.len())?;
        }
        let span = Span {
            header: 0,
            data: 0,
            end: raw.len(),
        };
        let mut frames = std::array::from_fn(|_| None);
        frames[0] = Some(Frame {
            scope: Scope::Group,
            span,
            cursor: ChildCursor::new(span.payload()),
            crc: None,
            checked: false,
        });
        Ok(Self {
            raw,
            pool: pool.clone(),
            limit: limits.metadata_nodes,
            maximum,
            highest: 0,
            location,
            frames,
            depth: 1,
            more: None,
            block: None,
            duration: None,
            discard: None,
            state: None,
            additions_seen: false,
            zero_reference: false,
            additions: CopyVec::new(pool),
            extensions: CopyVec::new(pool),
            references: CopyVec::new(pool),
            pending: None,
            queued: false,
            ids: BudgetVec::new(pool),
            heap: None,
            at: 0,
            crc: u32::MAX,
            phase: Phase::Parse,
            result: None,
            failed: false,
        })
    }
    pub(crate) fn set_location(&mut self, location: GroupLocation) -> Result<()> {
        location.validate(self.raw.len())?;
        self.location = Some(location);
        Ok(())
    }
    fn queue(&mut self, pending: Pending) -> Result<()> {
        let count = self
            .additions
            .len()
            .checked_add(self.extensions.len())
            .and_then(|n| n.checked_add(self.references.len()))
            .ok_or(Error::Overflow)?;
        if count >= self.limit {
            return Err(Error::Policy("packet source descriptors"));
        }
        self.pending = Some(pending);
        self.queued = false;
        self.phase = Phase::Flush;
        Ok(())
    }
    fn push_frame(&mut self, scope: Scope, span: Span) -> Result<()> {
        if self.depth == self.frames.len() {
            return Err(Error::Policy("packet source nesting"));
        }
        self.frames[self.depth] = Some(Frame {
            scope,
            span,
            cursor: ChildCursor::new(span.payload()),
            crc: None,
            checked: false,
        });
        self.depth += 1;
        Ok(())
    }
    fn child(&mut self, child: Child) -> Result<()> {
        let span = Span::from_child(&child);
        let frame = self.frames[self.depth - 1]
            .as_mut()
            .required("missing group frame")?;
        let scope = frame.scope;
        if child.id == 0xbf {
            if frame.crc.is_some() || child.header != frame.span.data || child.payload.len() != 4 {
                return Err(Error::Invalid("packet source CRC element"));
            }
            let value = u32::from_le_bytes(
                self.raw[child.payload]
                    .try_into()
                    .map_err(|_| Error::Truncated)?,
            );
            frame.crc = Some((span, value));
            return Ok(());
        }
        match (scope, child.id) {
            (Scope::Group, 0xa1) => {
                if self.block.replace(span).is_some() {
                    return Err(Error::Invalid("duplicate Block"));
                }
            }
            (Scope::Group, 0x9b) => {
                if self.duration.is_some() {
                    return Err(Error::Invalid("duplicate BlockDuration"));
                }
                self.duration = Some((span, ebml::uint(&self.raw[span.payload()])?));
            }
            (Scope::Group, 0x75a2) => {
                if self.discard.is_some() {
                    return Err(Error::Invalid("duplicate DiscardPadding"));
                }
                self.discard = Some((span, ebml::sint(&self.raw[span.payload()])?));
            }
            (Scope::Group, 0xfb) => {
                let value = ebml::sint(&self.raw[span.payload()])?;
                self.zero_reference |= value == 0;
                self.queue(Pending::Reference(value))?;
            }
            (Scope::Group, 0xa4) => {
                if self.state.replace(span).is_some() {
                    return Err(Error::Invalid("duplicate CodecState"));
                }
            }
            (Scope::Group, 0x75a1) => {
                if self.additions_seen {
                    return Err(Error::Invalid("duplicate BlockAdditions"));
                }
                self.additions_seen = true;
                self.push_frame(Scope::Additions, span)?;
            }
            (Scope::Additions, 0xa6) => {
                self.more = Some(More {
                    span,
                    id: 1,
                    id_field: None,
                    data: None,
                });
                self.push_frame(Scope::More(self.additions.len()), span)?;
            }
            (Scope::More(_), 0xee) => {
                let more = self.more.as_mut().required("missing BlockMore")?;
                if more.id_field.replace(span).is_some() {
                    return Err(Error::Invalid("duplicate BlockAddID"));
                }
                more.id = if span.data == span.end {
                    1
                } else {
                    ebml::uint(&self.raw[span.payload()])?
                };
                if more.id == 0 {
                    return Err(Error::Invalid("zero BlockAddID"));
                }
            }
            (Scope::More(_), 0xa5) => {
                if self
                    .more
                    .as_mut()
                    .required("missing BlockMore")?
                    .data
                    .replace(span)
                    .is_some()
                {
                    return Err(Error::Invalid("duplicate BlockAdditional"));
                }
            }
            _ => {
                let scope = match scope {
                    Scope::Group => ExtensionScope::Group,
                    Scope::Additions => ExtensionScope::Additions,
                    Scope::More(i) => ExtensionScope::More(i),
                };
                self.queue(Pending::Extension(Extension {
                    id: child.id,
                    scope,
                    span,
                }))?;
            }
        }
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let frame = self.frames[self.depth - 1]
            .take()
            .required("missing group frame")?;
        self.depth -= 1;
        match frame.scope {
            Scope::More(_) => {
                let more = self.more.take().required("missing BlockMore")?;
                let data = more.data.required("BlockMore without BlockAdditional")?;
                if more.id > self.maximum {
                    return Err(Error::Invalid("BlockAddID exceeds track maximum"));
                }
                self.highest = self.highest.max(more.id);
                self.queue(Pending::Addition(Addition {
                    id: more.id,
                    id_field: more.id_field,
                    more: more.span,
                    data,
                }))?;
            }
            Scope::Additions if self.additions.len() == 0 => {
                return Err(Error::Invalid("BlockAdditions without BlockMore"));
            }
            Scope::Additions => (),
            Scope::Group => {
                self.block.required("BlockGroup without Block")?;
                if self.zero_reference && self.references.len() != 1 {
                    return Err(Error::Invalid("zero ReferenceBlock with other references"));
                }
                self.phase = Phase::IdReserve;
            }
        }
        Ok(())
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<BlockGroupSource>> {
        if self.failed {
            return Err(Error::Invalid("group job requires replacement after error"));
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
    ) -> Result<ParseStep<BlockGroupSource>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Parse => {
                    let step = self.frames[self.depth - 1]
                        .as_mut()
                        .required("missing group frame")?
                        .cursor
                        .step(&self.raw, work, cancel)?;
                    match step {
                        ParseStep::Yield => return Ok(ParseStep::Yield),
                        ParseStep::Event(Some(child)) => self.child(child)?,
                        ParseStep::Event(None) => {
                            let frame = self.frames[self.depth - 1]
                                .as_ref()
                                .required("missing group frame")?;
                            if frame.crc.is_some() && !frame.checked {
                                self.at = frame.span.data;
                                self.crc = u32::MAX;
                                self.phase = Phase::Crc;
                            } else {
                                if !work.try_charge(1) {
                                    return Ok(ParseStep::Yield);
                                }
                                self.close()?;
                            }
                        }
                        _ => return Err(Error::Invalid("group parse transition")),
                    }
                }
                Phase::Flush => {
                    let pending = self.pending.required("missing group descriptor")?;
                    if !self.queued {
                        match pending {
                            Pending::Addition(v) => self.additions.push(v)?,
                            Pending::Extension(v) => self.extensions.push(v)?,
                            Pending::Reference(v) => self.references.push(v)?,
                        }
                        self.queued = true;
                    }
                    let step = match pending {
                        Pending::Addition(_) => self.additions.step(work, cancel)?,
                        Pending::Extension(_) => self.extensions.step(work, cancel)?,
                        Pending::Reference(_) => self.references.step(work, cancel)?,
                    };
                    match step {
                        ParseStep::Event(()) => {
                            self.pending = None;
                            self.queued = false;
                            self.phase = Phase::Parse;
                        }
                        other => return super::pending(other),
                    }
                }
                Phase::Crc => {
                    let frame = self.frames[self.depth - 1]
                        .as_ref()
                        .required("missing CRC frame")?;
                    let (skip, expected) = frame.crc.required("missing packet CRC")?;
                    if self.at == frame.span.end {
                        if !self.crc != expected {
                            return Err(Error::Invalid("packet source CRC mismatch"));
                        }
                        self.frames[self.depth - 1]
                            .as_mut()
                            .required("missing CRC frame")?
                            .checked = true;
                        self.phase = Phase::Parse;
                        continue;
                    }
                    if self.at == skip.header {
                        self.at = skip.end;
                        continue;
                    }
                    let end = if self.at < skip.header {
                        skip.header
                    } else {
                        frame.span.end
                    };
                    let count = work.take((end - self.at).min(2048) as u64) as usize;
                    if count == 0 {
                        return Ok(ParseStep::Yield);
                    }
                    for byte in &self.raw[self.at..self.at + count] {
                        self.crc ^= u32::from(*byte);
                        for _ in 0..8 {
                            self.crc =
                                (self.crc >> 1) ^ if self.crc & 1 == 1 { 0xedb88320 } else { 0 };
                        }
                    }
                    self.at += count;
                }
                Phase::IdReserve => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.ids.try_reserve_exact(self.additions.len())?;
                    self.at = 0;
                    self.phase = Phase::IdFill;
                }
                Phase::IdFill => {
                    if self.at == self.additions.len() {
                        self.heap =
                            Some(crate::metadata::typed_document::Heap::new(self.ids.len()));
                        self.phase = Phase::IdSort;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.ids
                        .try_push(self.additions.as_slice()[self.at].id)
                        .map_err(|e| e.error)?;
                    self.at += 1;
                }
                Phase::IdSort => {
                    if !self
                        .heap
                        .as_mut()
                        .required("missing addition-ID sort")?
                        .step(self.ids.as_mut_slice(), work, cancel)?
                    {
                        return Ok(ParseStep::Yield);
                    }
                    self.at = 0;
                    self.phase = Phase::IdCheck;
                }
                Phase::IdCheck => {
                    if self.at + 1 >= self.ids.len() {
                        self.ids = BudgetVec::new(&self.pool);
                        self.heap = None;
                        self.phase = Phase::Seal;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.ids.as_slice()[self.at] == self.ids.as_slice()[self.at + 1] {
                        return Err(Error::Invalid("duplicate effective BlockAddID"));
                    }
                    self.at += 1;
                }
                Phase::Seal => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let charge = self
                        .pool
                        .reserve(size_of::<Data>() + 2 * size_of::<usize>())?;
                    let additions =
                        std::mem::replace(&mut self.additions, CopyVec::new(&self.pool))
                            .finish()?;
                    let extensions =
                        std::mem::replace(&mut self.extensions, CopyVec::new(&self.pool))
                            .finish()?;
                    let references =
                        std::mem::replace(&mut self.references, CopyVec::new(&self.pool))
                            .finish()?;
                    self.result = Some(BlockGroupSource(Arc::new(Data {
                        raw: self.raw.clone(),
                        additions,
                        extensions,
                        references,
                        block: self.block.required("BlockGroup without Block")?,
                        duration: self.duration,
                        discard: self.discard,
                        state: self.state,
                        highest: self.highest,
                        location: self.location,
                        pool: self.pool.clone(),
                        charge,
                    })));
                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    return Ok(ParseStep::Event(
                        self.result
                            .as_ref()
                            .required("missing source group")?
                            .clone(),
                    ));
                }
            }
        }
    }
}

impl super::usage::Storage for BlockGroupSource {
    fn storage_slots(&self) -> Result<usize> {
        Ok(2)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<super::usage::Owner>> {
        match index {
            0 => Ok(Some(super::usage::Owner::Group(self.clone()))),
            1 => super::usage::Storage::storage_owner(&self.0.raw, 0),
            _ => Ok(None),
        }
    }
}
super::usage::storage!(GroupJob, |s| [
    s.raw,
    s.additions,
    s.extensions,
    s.references,
    s.ids,
    s.result
]);
