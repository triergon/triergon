use super::Piece;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

pub(super) const LEADING_VIDEO_BYTES: Error = Error::Invalid("Annex B leading bytes");

#[derive(Default)]
pub(super) struct UnframedVideoScan {
    piece: usize,
    span: usize,
    offset: usize,
    zeroes: u8,
}

impl UnframedVideoScan {
    pub(super) fn step(
        &mut self,
        pieces: &[Piece],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while let Some(piece) = pieces.get(self.piece) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let Some(span) = piece.source.pes().spans().get(self.span) else {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.piece += 1;
                self.span = 0;
                continue;
            };
            let bytes = span.as_ref();
            if self.offset == bytes.len() {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.span += 1;
                self.offset = 0;
                continue;
            }
            let count = work.take((bytes.len() - self.offset).min(4096) as u64) as usize;
            if count == 0 {
                return Ok(false);
            }
            for byte in &bytes[self.offset..self.offset + count] {
                if *byte == 0 {
                    self.zeroes = (self.zeroes + 1).min(2);
                } else {
                    if *byte == 1 && self.zeroes == 2 {
                        return Err(LEADING_VIDEO_BYTES);
                    }
                    self.zeroes = 0;
                }
            }
            self.offset += count;
        }
        Ok(true)
    }
}
