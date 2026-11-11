//! The blocks of a WebVTT document as the `webvtt` demuxer cuts it.
//!
//! CR and LF are line-break characters. Those at the start of a block are
//! skipped, and those at its end are not part of it. A run of them between
//! two other bytes ends the block once it holds two line ends: an LF is one,
//! and so is either character after a CR, the CR being the last line-break
//! character of the block so far, wherever it was. A NUL or the end of the
//! document ends a block too, and a block with nothing in it ends the
//! document.

use super::lines::Span;
use triergon_core::WorkBudget;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Block {
    Text(Span),
    /// The document ended; every later call returns this again.
    End,
}

#[derive(Debug)]
pub(super) struct Blocks {
    /// Next byte to scan.
    cursor: usize,
    /// The block being scanned, from its first byte to the end of its last.
    block: Option<Span>,
    /// Line ends in the run of line-break characters since the last byte.
    line_ends: u8,
    /// The last line-break character of the block was a CR.
    after_cr: bool,
    ended: bool,
}

impl Blocks {
    pub(super) fn new() -> Self {
        Self {
            cursor: 0,
            block: None,
            line_ends: 0,
            after_cr: false,
            ended: false,
        }
    }

    /// The next block, or `None` when `work` runs out first. Each byte
    /// scanned costs one unit.
    pub(super) fn next(&mut self, source: &[u8], work: &mut WorkBudget) -> Option<Block> {
        if self.ended {
            return Some(Block::End);
        }
        while self.cursor < source.len() {
            if !work.try_charge(1) {
                return None;
            }
            let at = self.cursor;
            self.cursor += 1;
            match source[at] {
                0 => return Some(self.take()),
                byte @ (b'\r' | b'\n') if self.block.is_some() => {
                    self.line_ends += u8::from(byte == b'\n' || self.after_cr);
                    self.after_cr = byte == b'\r';
                    if self.line_ends == 2 {
                        return Some(self.take());
                    }
                }
                b'\r' | b'\n' => {}
                _ => {
                    self.line_ends = 0;
                    let block = self.block.get_or_insert(Span { start: at, end: at });
                    block.end = at + 1;
                }
            }
        }
        Some(self.take())
    }

    fn take(&mut self) -> Block {
        self.line_ends = 0;
        self.after_cr = false;
        let block = self.block.take();
        self.ended = block.is_none();
        block.map_or(Block::End, Block::Text)
    }
}
