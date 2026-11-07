//! Packet reassembly over the pages of one logical stream.
use crate::page::PageHeader;
use triergon_core::{Budget, BudgetVec, Error, Result};

/// A packet completed by a page.
#[derive(Debug)]
pub struct Completed {
    /// The packet bytes, valid until the next call on the assembler.
    pub range: core::ops::Range<usize>,
    /// Whether this is the last packet completed on its page, the one the page's granule
    /// position describes.
    pub last_on_page: bool,
}

/// Reassembles the packets of one logical stream from its pages, charging held bytes to
/// a budget. Pages must arrive in sequence; a gap drops the packet in progress.
#[derive(Debug)]
pub struct Assembler {
    /// The packet that continues onto the next page.
    partial: BudgetVec<u8>,
    /// Completed packets of the current page, back to back.
    done: BudgetVec<u8>,
    next_sequence: Option<u32>,
    /// Bytes of the packet in progress were lost (a sequence gap or an unexpected
    /// continuation); its remainder is skipped.
    broken: bool,
    max_packet: usize,
}

impl Assembler {
    /// An assembler holding packets of at most `max_packet` bytes.
    pub fn new(budget: &Budget, max_packet: usize) -> Assembler {
        Assembler {
            partial: BudgetVec::new(budget),
            done: BudgetVec::new(budget),
            next_sequence: None,
            broken: false,
            max_packet,
        }
    }

    /// Forget the packet in progress and the expected sequence number, as after a seek.
    pub fn reset(&mut self) {
        self.partial.clear();
        self.done.clear();
        self.next_sequence = None;
        self.broken = false;
    }

    /// Whether a packet continues past the last page pushed.
    pub fn in_progress(&self) -> bool {
        !self.partial.is_empty()
    }

    /// Add one page (header and body) and append the packets it completes to `out`.
    /// Returns whether the page lost data: a sequence gap, or a continuation with no
    /// packet in progress.
    pub fn push(
        &mut self,
        header: &PageHeader,
        body: &[u8],
        out: &mut Vec<Completed>,
    ) -> Result<bool> {
        out.clear();
        self.done.clear();
        let mut lost = false;
        if let Some(expected) = self.next_sequence
            && header.sequence != expected
        {
            lost = true;
            if !self.partial.is_empty() {
                self.partial.clear();
                self.broken = true;
            }
        }
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        let mut first = true;
        for (range, complete) in header.pieces() {
            let piece = body.get(range).ok_or(Error::Truncated)?;
            if first && header.continued() && self.partial.is_empty() && !self.broken {
                // RFC 3533: a continuation without its start is skipped.
                lost |= self.next_sequence.is_some();
                self.broken = true;
            }
            if first && !header.continued() && (self.in_progress() || self.broken) {
                // The previous packet never finished.
                self.partial.clear();
                self.broken = false;
                lost = true;
            }
            first = false;
            if self.broken {
                if complete {
                    self.broken = false;
                }
                continue;
            }
            if self.partial.len() + piece.len() > self.max_packet {
                return Err(Error::Limit {
                    requested: self.partial.len() + piece.len(),
                    maximum: self.max_packet,
                });
            }
            self.partial.try_extend_from_slice(piece)?;
            if complete {
                let start = self.done.len();
                self.done.try_extend_from_slice(&self.partial)?;
                self.partial.clear();
                out.push(Completed {
                    range: start..self.done.len(),
                    last_on_page: false,
                });
            }
        }
        if let Some(last) = out.last_mut() {
            last.last_on_page = true;
        }
        Ok(lost)
    }

    /// The bytes of a [`Completed`] packet from the last push.
    pub fn packet(&self, completed: &Completed) -> &[u8] {
        &self.done[completed.range.clone()]
    }
}
