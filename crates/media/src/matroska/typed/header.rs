//! Bounded EBML document and SeekHead syntax. No blocking parser calls.
use super::{
    copy_vec::CopyVec,
    native_additions::BlockAdditionContainer,
    syntax::{ChildCursor, check},
};
use crate::matroska::typed::syntax::Required;
use crate::{Error, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};
#[derive(Debug)]
pub(crate) struct EbmlJob {
    source: SharedBytes,
    cursor: ChildCursor,
    doctype: Option<BlockAdditionContainer>,
    done: bool,
}
impl EbmlJob {
    pub(crate) fn new(source: SharedBytes) -> Self {
        let end = source.len();
        Self {
            source,
            cursor: ChildCursor::new(0..end),
            doctype: None,
            done: false,
        }
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<BlockAdditionContainer>> {
        loop {
            check(cancel)?;
            if self.done {
                return Ok(ParseStep::Event(
                    self.doctype.ok_or(Error::Unsupported("document type"))?,
                ));
            }
            match self.cursor.step(&self.source, work, cancel)? {
                ParseStep::Yield => return Ok(ParseStep::Yield),
                ParseStep::Event(None) => {
                    if self.doctype.is_none() {
                        return Err(Error::Unsupported("document type"));
                    }
                    self.done = true;
                }
                ParseStep::Event(Some(child)) => {
                    let bytes = &self.source[child.payload];
                    match child.id {
                        0x4282 => {
                            self.doctype = match bytes {
                                b"matroska" => Some(BlockAdditionContainer::Matroska),
                                b"webm" => Some(BlockAdditionContainer::WebM),
                                _ => None,
                            };
                        }
                        0x4285 if ebml::uint(bytes)? > 4 => {
                            return Err(Error::Unsupported("DocTypeReadVersion"));
                        }
                        0x42f7 if ebml::uint(bytes)? > 1 => {
                            return Err(Error::Unsupported("EBMLReadVersion"));
                        }
                        0x42f2 if ebml::uint(bytes)? > 4 => {
                            return Err(Error::Unsupported("EBML ID width"));
                        }
                        0x42f3 if ebml::uint(bytes)? > 8 => {
                            return Err(Error::Unsupported("EBML size width"));
                        }
                        _ => (),
                    }
                }
                _ => return Err(Error::Invalid("invalid EBML document transition")),
            }
        }
    }
}
#[derive(Debug)]
enum Phase {
    Root,
    Entry,
    Push,
    Done,
}
#[derive(Debug)]
pub(crate) struct SeekHeadJob {
    source: SharedBytes,
    root: ChildCursor,
    entry: Option<ChildCursor>,
    identifier: Option<u64>,
    position: Option<u64>,
    segment: u64,
    old_count: usize,
    output: Option<CopyVec<(u64, u64)>>,
    phase: Phase,
}
impl SeekHeadJob {
    pub(crate) fn new(source: SharedBytes, segment: u64, old_count: usize, pool: &Budget) -> Self {
        let end = source.len();
        Self {
            source,
            root: ChildCursor::new(0..end),
            entry: None,
            identifier: None,
            position: None,
            segment,
            old_count,
            output: Some(CopyVec::new(pool)),
            phase: Phase::Root,
        }
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<BudgetVec<(u64, u64)>>> {
        loop {
            check(cancel)?;
            match self.phase {
                Phase::Root => match self.root.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        let output = self.output.take().required("missing SeekHead arena")?;
                        self.phase = Phase::Done;
                        return Ok(ParseStep::Event(output.finish()?));
                    }
                    ParseStep::Event(Some(child)) => {
                        if child.id == 0x4dbb {
                            self.identifier = None;
                            self.position = None;
                            self.entry = Some(ChildCursor::new(child.payload));
                            self.phase = Phase::Entry;
                        }
                    }
                    _ => return Err(Error::Invalid("invalid SeekHead transition")),
                },
                Phase::Entry => match self.entry.as_mut().required("missing Seek entry")?.step(
                    &self.source,
                    work,
                    cancel,
                )? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(Some(child)) => match child.id {
                        0x53ab => self.identifier = Some(ebml::uint(&self.source[child.payload])?),
                        0x53ac => self.position = Some(ebml::uint(&self.source[child.payload])?),
                        _ => (),
                    },
                    ParseStep::Event(None) => {
                        let (id, relative) = self
                            .identifier
                            .zip(self.position)
                            .required("incomplete Seek entry")?;
                        if matches!(
                            id,
                            0x1549a966
                                | 0x1654ae6b
                                | 0x1c53bb6b
                                | 0x114d9b74
                                | 0x1941a469
                                | 0x1043a770
                                | 0x1254c367
                        ) {
                            let output = self.output.as_mut().required("missing SeekHead arena")?;
                            if self
                                .old_count
                                .checked_add(output.len())
                                .ok_or(Error::Overflow)?
                                >= 256
                            {
                                return Err(Error::Policy("metadata seek targets"));
                            }
                            output.push((
                                id,
                                self.segment.checked_add(relative).ok_or(Error::Overflow)?,
                            ))?;
                            self.phase = Phase::Push;
                        } else {
                            self.phase = Phase::Root;
                        }
                    }
                    _ => return Err(Error::Invalid("invalid Seek entry transition")),
                },
                Phase::Push => match self
                    .output
                    .as_mut()
                    .required("missing SeekHead arena")?
                    .step(work, cancel)?
                {
                    ParseStep::Event(()) => self.phase = Phase::Root,
                    other => return super::pending(other),
                },
                Phase::Done => return Ok(ParseStep::Finished),
            }
        }
    }
}

super::usage::storage!(EbmlJob, |s| [s.source]);
super::usage::storage!(SeekHeadJob, |s| [s.source, s.output]);
