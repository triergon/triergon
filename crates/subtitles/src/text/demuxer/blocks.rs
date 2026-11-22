//! The blocks of a WebVTT document as the `webvtt` demuxer cuts it.
//!
//! CR and LF are the line-break bytes. Line breaks before a block are
//! skipped: a block starts at the next other byte. It ends before a run of
//! line-break bytes that ends blocks, at a NUL byte, or at the end of the
//! document; the bytes that end it are not part of it.
//!
//! A run, a maximal sequence of line-break bytes, leaves the block open when
//! it is LF, LF CR or CR, and also when it is CR CR, CR LF or CR LF CR and
//! the previous run in the same block ended with LF or there was none. Any
//! other run ends the block: LF LF always does, CR CR only after a run that
//! ended with CR, and so does every run of four or more bytes.
//!
//! A NUL byte ends the block it is in, and the next block starts after it.
//! EOF or a NUL where a block would start ends the document. A UTF-8
//! byte-order mark gets no special treatment here.

use super::lines::Span;
use triergon_core::WorkBudget;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Block {
    /// The bytes of one block, as a range of the document.
    Text(Span),
    /// No more blocks remain. Every later call also returns this.
    End,
}

#[derive(Debug)]
pub(super) struct Blocks {
    /// Next byte to scan.
    cursor: usize,
    /// Start of the block being cut; `None` between blocks.
    start: Option<usize>,
    /// Whether the last run of line-break bytes in the block ended with CR.
    after_cr: bool,
    ended: bool,
}

const CR: u8 = b'\r';
const LF: u8 = b'\n';

fn is_break(byte: u8) -> bool {
    byte == CR || byte == LF
}

/// The run of line-break bytes starting at `source[at]`, as its length if
/// the run leaves the block open, or `None` when it ends the block.
fn open_run(source: &[u8], at: usize, after_cr: bool) -> Option<usize> {
    // Every run that leaves a block open is at most three bytes long.
    let window = &source[at..source.len().min(at + 4)];
    let length = window.iter().take_while(|&&byte| is_break(byte)).count();
    match &window[..length] {
        b"\n" | b"\n\r" | b"\r" => Some(length),
        b"\r\r" | b"\r\n" | b"\r\n\r" if !after_cr => Some(length),
        _ => None,
    }
}

impl Blocks {
    pub(super) fn new() -> Self {
        Self {
            cursor: 0,
            start: None,
            after_cr: false,
            ended: false,
        }
    }

    /// The next block, or `None` when `work` runs out first. Each byte scanned
    /// costs one unit.
    pub(super) fn next(&mut self, source: &[u8], work: &mut WorkBudget) -> Option<Block> {
        loop {
            if self.ended {
                return Some(Block::End);
            }
            let Some(start) = self.start else {
                // Between blocks: skip line breaks.
                let byte = source.get(self.cursor).copied().unwrap_or(0);
                if byte == 0 {
                    self.ended = true;
                    return Some(Block::End);
                }
                if is_break(byte) {
                    if !work.try_charge(1) {
                        return None;
                    }
                    self.cursor += 1;
                } else {
                    self.start = Some(self.cursor);
                    self.after_cr = false;
                }
                continue;
            };
            let rest = &source[self.cursor..];
            if rest.is_empty() {
                self.start = None;
                return Some(Block::Text(Span {
                    start,
                    end: source.len(),
                }));
            }
            let budget = usize::try_from(work.remaining()).unwrap_or(usize::MAX);
            let window = &rest[..rest.len().min(budget)];
            if window.is_empty() {
                return None;
            }
            let Some(offset) = window.iter().position(|&byte| byte == 0 || is_break(byte)) else {
                work.take(window.len() as u64);
                self.cursor += window.len();
                continue;
            };
            work.take(offset as u64);
            let at = self.cursor + offset;
            if source[at] == 0 {
                work.take(1);
                self.cursor = at + 1;
                self.start = None;
                return Some(Block::Text(Span { start, end: at }));
            }
            let Some(length) = open_run(source, at, self.after_cr) else {
                // The run is skipped, and paid for, before the next block.
                self.cursor = at;
                self.start = None;
                return Some(Block::Text(Span { start, end: at }));
            };
            work.take(length as u64);
            self.cursor = at + length;
            self.after_cr = source[self.cursor - 1] == CR;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, Blocks, Span};
    use triergon_core::WorkBudget;

    #[test]
    fn eof_or_nul_between_blocks_ends_without_work() {
        for source in [b"X\n\n".as_slice(), b"X\n\n\0ignored"] {
            let mut blocks = Blocks::new();
            let mut work = WorkBudget::new(1);
            // Seeing the separator requires another poll after reading X.
            assert_eq!(blocks.next(source, &mut work), None);
            let mut work = WorkBudget::new(2);
            assert_eq!(
                blocks.next(source, &mut work),
                Some(Block::Text(Span { start: 0, end: 1 }))
            );
            assert_eq!(work.spent(), 0);
            assert_eq!(blocks.next(source, &mut work), Some(Block::End));
            assert_eq!(work.spent(), 2);
            assert_eq!(blocks.next(source, &mut work), Some(Block::End));
        }
    }

    #[test]
    fn a_nul_inside_a_block_delivers_it_and_allows_another_block() {
        let source = b"X\0Y";
        let mut blocks = Blocks::new();
        let mut work = WorkBudget::new(3);
        for span in [Span { start: 0, end: 1 }, Span { start: 2, end: 3 }] {
            assert_eq!(blocks.next(source, &mut work), Some(Block::Text(span)));
        }
        assert_eq!(work.spent(), 3);
        assert_eq!(blocks.next(source, &mut work), Some(Block::End));
    }
}
