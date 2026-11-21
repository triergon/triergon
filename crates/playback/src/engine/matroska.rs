//! The Matroska engine: the reader's own parse steps.

use crate::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{SourceId, WorkBudget};
use triergon_matroska::{Event as MkvEvent, ParseStep, Reader};
#[derive(Debug)]
pub struct MkvEngine {
    pub(crate) reader: Reader,
    pub(crate) source: SourceId,
}

impl MkvEngine {
    pub(crate) fn step(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<ParseStep<MkvEvent>> {
        self.reader.step_parse(w, c)
    }
}

impl std::ops::Deref for MkvEngine {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        &self.reader
    }
}
