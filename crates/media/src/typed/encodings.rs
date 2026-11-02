//! Intended child of content_encoding. Reuses Decoder/DecodeStep/Encodings.
//! Eight entries are the frozen policy, not a new unbounded recursive model.
use super::{DecodeStep, Decoder, Encodings};
use crate::matroska::typed::syntax::Required;
use crate::matroska::typed::syntax::{ChildCursor, check};
use crate::zlib::typed_restore::CopyJob;
use crate::{Error, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, BudgetVec, WorkBudget};
#[derive(Debug)]
struct Entry {
    order: u64,
    scope: u64,
    algorithm: u64,
    settings: Range<usize>,
}
#[derive(Debug)]
enum Phase {
    Root,
    Entry,
    Compression,
    CheckOrder,
    Sort,
    Inspect,
    Build,
    Header,
    Append,
    PrefixSize,
    PrefixReserve,
    PrefixCopy,
    PrefixFreeze,
    Done,
}
#[derive(Debug)]
pub(crate) struct EncodingsJob {
    source: SharedBytes,
    pool: Budget,
    limit: usize,
    root: ChildCursor,
    entry: Option<ChildCursor>,
    compression: Option<ChildCursor>,
    compression_range: Option<Range<usize>>,
    entries: [Option<Entry>; 8],
    count: usize,
    order: u64,
    scope: u64,
    kind: u64,
    algorithm: u64,
    settings: Range<usize>,
    compare: usize,
    phase: Phase,
    sort_i: usize,
    sort_j: usize,
    inspect: usize,
    headers_only: bool,
    build: usize,
    header: SharedBytes,
    copy: Option<CopyJob>,
    out: Encodings,
    prefix: Option<BudgetVec<u8>>,
    prefix_size: usize,
    prefix_at: usize,
    prefix_entry: usize,
    failed: bool,
}
impl EncodingsJob {
    pub fn new(
        source: SharedBytes,
        range: Range<usize>,
        limit: usize,
        pool: &Budget,
    ) -> Result<Self> {
        if !source.belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        if range.start > range.end || range.end > source.len() {
            return Err(Error::Truncated);
        }
        Ok(Self {
            source,
            pool: pool.clone(),
            limit,
            root: ChildCursor::new(range),
            entry: None,
            compression: None,
            compression_range: None,
            entries: std::array::from_fn(|_| None),
            count: 0,
            order: 0,
            scope: 1,
            kind: 0,
            algorithm: 0,
            settings: 0..0,
            compare: 0,
            phase: Phase::Root,
            sort_i: 1,
            sort_j: 1,
            inspect: 0,
            headers_only: true,
            build: 0,
            header: SharedBytes::default(),
            copy: None,
            out: Encodings::default(),
            prefix: Some(BudgetVec::new(pool)),
            prefix_size: 0,
            prefix_at: 0,
            prefix_entry: 0,
            failed: false,
        })
    }
    pub fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Encodings>> {
        if self.failed {
            return Err(Error::Invalid(
                "ContentEncodings requires replacement after error",
            ));
        }
        let result = self.advance(work, cancel);
        match result {
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
    ) -> Result<ParseStep<Encodings>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Root => match self.root.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        if self.count == 0 {
                            return Err(Error::Invalid("empty ContentEncodings"));
                        }
                        self.phase = Phase::Sort;
                    }
                    ParseStep::Event(Some(child)) => {
                        if child.id == 0x6240 {
                            if self.count == 8 {
                                return Err(Error::Limit("content encoding count"));
                            }
                            self.order = 0;
                            self.scope = 1;
                            self.kind = 0;
                            self.compression_range = None;
                            self.entry = Some(ChildCursor::new(child.payload));
                            self.phase = Phase::Entry;
                        }
                    }
                    _ => return Err(Error::Invalid("invalid ContentEncodings transition")),
                },
                Phase::Entry => match self
                    .entry
                    .as_mut()
                    .required("missing encoding entry")?
                    .step(&self.source, work, cancel)?
                {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(Some(child)) => match child.id {
                        0x5031 => self.order = ebml::uint(&self.source[child.payload])?,
                        0x5032 => self.scope = ebml::uint(&self.source[child.payload])?,
                        0x5033 => self.kind = ebml::uint(&self.source[child.payload])?,
                        0x5034 => {
                            if self.compression_range.replace(child.payload).is_some() {
                                return Err(Error::Invalid("duplicate ContentCompression"));
                            }
                        }
                        0x5035 => return Err(Error::Unsupported("encrypted track")),
                        _ => (),
                    },
                    ParseStep::Event(None) => {
                        if self.kind != 0 {
                            return Err(Error::Unsupported("content encoding type"));
                        }
                        if self.scope == 0 {
                            return Err(Error::Invalid("empty content encoding scope"));
                        }
                        if self.scope & !3 != 0 {
                            return Err(Error::Unsupported("next-content encoding scope"));
                        }
                        let range = self
                            .compression_range
                            .take()
                            .required("missing ContentCompression")?;
                        self.compression = Some(ChildCursor::new(range));
                        self.algorithm = 0;
                        self.settings = 0..0;
                        self.phase = Phase::Compression;
                    }
                    _ => return Err(Error::Invalid("invalid encoding entry transition")),
                },
                Phase::Compression => match self
                    .compression
                    .as_mut()
                    .required("missing compression cursor")?
                    .step(&self.source, work, cancel)?
                {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(Some(child)) => match child.id {
                        0x4254 => self.algorithm = ebml::uint(&self.source[child.payload])?,
                        0x4255 => self.settings = child.payload,
                        _ => (),
                    },
                    ParseStep::Event(None) => {
                        if !matches!(self.algorithm, 0 | 3) {
                            return Err(Error::Unsupported("content compression algorithm"));
                        }
                        self.compare = 0;
                        self.phase = Phase::CheckOrder;
                    }
                    _ => return Err(Error::Invalid("invalid compression transition")),
                },
                Phase::CheckOrder => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.compare < self.count {
                        if self.entries[self.compare]
                            .as_ref()
                            .required("missing encoding entry")?
                            .order
                            == self.order
                        {
                            return Err(Error::Invalid("duplicate content encoding order"));
                        }
                        self.compare += 1;
                        continue;
                    }
                    self.entries[self.count] = Some(Entry {
                        order: self.order,
                        scope: self.scope,
                        algorithm: self.algorithm,
                        settings: self.settings.clone(),
                    });
                    self.count += 1;
                    self.entry = None;
                    self.compression = None;
                    self.phase = Phase::Root;
                }
                Phase::Sort => {
                    if self.sort_i == self.count {
                        self.phase = Phase::Inspect;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    if self.sort_j > 0
                        && self.entries[self.sort_j - 1]
                            .as_ref()
                            .required("missing encoding entry")?
                            .order
                            > self.entries[self.sort_j]
                                .as_ref()
                                .required("missing encoding entry")?
                                .order
                    {
                        self.entries.swap(self.sort_j - 1, self.sort_j);
                        self.sort_j -= 1;
                    } else {
                        self.sort_i += 1;
                        self.sort_j = self.sort_i;
                    }
                }
                Phase::Inspect => {
                    if self.inspect == self.count {
                        self.build = self.count;
                        self.phase = Phase::Build;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let e = self.entries[self.inspect]
                        .as_ref()
                        .required("missing encoding entry")?;
                    self.headers_only &= e.scope & 1 == 0 || e.algorithm == 3;
                    self.inspect += 1;
                }
                Phase::Build => {
                    if self.build == 0 {
                        self.phase = if self.headers_only {
                            Phase::PrefixSize
                        } else {
                            Phase::Done
                        };
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let e = self.entries[self.build - 1]
                        .as_ref()
                        .required("missing encoding entry")?;
                    if e.settings.len() > self.limit {
                        return Err(Error::Limit("content encoding settings"));
                    }
                    self.header = SharedBytes::default();
                    if e.algorithm == 3
                        && (e.scope & 2 != 0 || e.scope & 1 != 0 && !self.headers_only)
                        && !e.settings.is_empty()
                    {
                        self.copy = Some(CopyJob::new(
                            SharedBytes::default(),
                            self.source.slice(e.settings.clone())?,
                            self.limit,
                            &self.pool,
                        )?);
                        self.phase = Phase::Header;
                    } else {
                        self.phase = Phase::Append;
                    }
                }
                Phase::Header => match self
                    .copy
                    .as_mut()
                    .required("missing encoding header copy")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(header) => {
                        self.header = header;
                        self.copy = None;
                        self.phase = Phase::Append;
                    }
                    other => return crate::matroska::typed::pending(other),
                },
                Phase::Append => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let e = self.entries[self.build - 1]
                        .as_ref()
                        .required("missing encoding entry")?;
                    if e.scope & 2 != 0 || e.scope & 1 != 0 && !self.headers_only {
                        let step = if e.algorithm == 0 {
                            DecodeStep::Zlib
                        } else {
                            DecodeStep::Header(self.header.clone())
                        };
                        if e.scope & 2 != 0 {
                            Decoder::append(&mut self.out.private, step.clone())?;
                        }
                        if e.scope & 1 != 0 && !self.headers_only {
                            Decoder::append(&mut self.out.frame, step)?;
                        }
                    }
                    self.header = SharedBytes::default();
                    self.build -= 1;
                    self.phase = Phase::Build;
                }
                Phase::PrefixSize => {
                    if self.prefix_entry == self.count {
                        self.prefix_entry = 0;
                        self.phase = Phase::PrefixReserve;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    let e = self.entries[self.prefix_entry]
                        .as_ref()
                        .required("missing encoding entry")?;
                    if e.scope & 1 != 0 {
                        self.prefix_size = self
                            .prefix_size
                            .checked_add(e.settings.len())
                            .ok_or(Error::Overflow)?;
                        if self.prefix_size > self.limit {
                            return Err(Error::Limit("restored header bytes"));
                        }
                    }
                    self.prefix_entry += 1;
                }
                Phase::PrefixReserve => {
                    if self.prefix_size == 0 {
                        self.phase = Phase::Done;
                        continue;
                    }
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    self.prefix
                        .as_mut()
                        .required("missing prefix builder")?
                        .try_reserve_exact(self.prefix_size)?;
                    self.phase = Phase::PrefixCopy;
                }
                Phase::PrefixCopy => {
                    if self.prefix_entry == self.count {
                        self.phase = Phase::PrefixFreeze;
                        continue;
                    }
                    let e = self.entries[self.prefix_entry]
                        .as_ref()
                        .required("missing encoding entry")?;
                    if e.scope & 1 == 0 || self.prefix_at == e.settings.len() {
                        self.prefix_entry += 1;
                        self.prefix_at = 0;
                        continue;
                    }
                    let count =
                        work.take((e.settings.len() - self.prefix_at).min(8192) as u64) as usize;
                    if count == 0 {
                        return Ok(ParseStep::Yield);
                    }
                    let start = e.settings.start + self.prefix_at;
                    self.prefix
                        .as_mut()
                        .required("missing prefix builder")?
                        .try_extend_from_slice(&self.source[start..start + count])?;
                    self.prefix_at += count;
                }
                Phase::PrefixFreeze => {
                    if !work.try_charge(1) {
                        return Ok(ParseStep::Yield);
                    }
                    match self
                        .prefix
                        .take()
                        .required("missing prefix builder")?
                        .freeze()
                    {
                        Ok(bytes) => {
                            self.out.frame_prefix = SharedBytes::from_core(bytes);
                            self.phase = Phase::Done;
                        }
                        Err(error) => {
                            self.prefix = Some(error.buffer);
                            return Err(error.error.into());
                        }
                    }
                }
                Phase::Done => return Ok(ParseStep::Event(self.out.clone())),
            }
        }
    }
}

crate::matroska::typed::usage::storage!(EncodingsJob, |s| [
    s.source, s.header, s.copy, s.out, s.prefix
]);
