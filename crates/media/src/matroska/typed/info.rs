//! TimestampScale projection, NOT a replacement for full Document validation.
//! The future dispatcher must pair this with the independently completed catalogue job.
use super::syntax::{ChildCursor, check};
use crate::{Error, Result, ebml, incremental::ParseStep, storage::SharedBytes};
use std::sync::atomic::AtomicBool;
use triergon_core::WorkBudget;
#[derive(Debug, Clone)]
pub(crate) struct InfoProjection {
    pub timestamp_scale_ns: u64,
    pub source: SharedBytes,
}
#[derive(Debug)]
pub(crate) struct InfoJob {
    source: SharedBytes,
    cursor: ChildCursor,
    scale: u64,
    seen: bool,
    failed: bool,
    done: bool,
}
impl InfoJob {
    pub(crate) fn new(source: SharedBytes) -> Self {
        let end = source.len();
        Self {
            source,
            cursor: ChildCursor::new(0..end),
            scale: 1_000_000,
            seen: false,
            failed: false,
            done: false,
        }
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<InfoProjection>> {
        if self.failed {
            return Err(Error::Invalid(
                "Info projection requires replacement after error",
            ));
        }
        let result = self.advance(work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<InfoProjection>> {
        loop {
            check(cancel)?;
            if self.done {
                return Ok(ParseStep::Event(InfoProjection {
                    timestamp_scale_ns: self.scale,
                    source: self.source.clone(),
                }));
            }
            match self.cursor.step(&self.source, work, cancel)? {
                ParseStep::Yield => return Ok(ParseStep::Yield),
                ParseStep::Event(None) => self.done = true,
                ParseStep::Event(Some(child)) => {
                    if child.id == 0x2ad7b1 {
                        if self.seen {
                            return Err(Error::Invalid("duplicate metadata field"));
                        }
                        let raw = &self.source[child.payload];
                        self.scale = if raw.is_empty() {
                            1_000_000
                        } else {
                            ebml::uint(raw)?
                        };
                        if self.scale == 0 {
                            return Err(Error::Invalid("zero TimestampScale"));
                        }
                        self.seen = true;
                    }
                }
                _ => return Err(Error::Invalid("invalid child projection transition")),
            }
        }
    }
}
