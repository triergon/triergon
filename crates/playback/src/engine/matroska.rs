//! The Matroska engine: the reader's own parse steps.

use crate::{Result, SeekIntent, TrackKey};
use std::sync::atomic::AtomicBool;
use triergon_core::Error;
use triergon_core::{SourceId, WorkBudget};
use triergon_matroska::{Event as MkvEvent, ParseStep, Reader};
/// The Matroska reader as an engine: the engine of
/// [`Engine::Matroska`](crate::Engine::Matroska).
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
    /// Repositions the reader at the Cue of the video track before the target.
    pub(crate) fn begin_seek(&mut self, intent: &SeekIntent) -> Result<()> {
        let TrackKey::Matroska(number) = intent.track else {
            return Err(Error::Invalid("seek track format"));
        };
        let at = self
            .reader
            .tracks()
            .iter()
            .position(|t| t.number == number && t.kind == 1)
            .ok_or(Error::Invalid("seek needs source video track"))?;
        self.reader.seek_for_playback(
            at,
            intent
                .target
                .to_nanoseconds(triergon_core::Rounding::Floor)?,
        )?;
        Ok(())
    }
}
