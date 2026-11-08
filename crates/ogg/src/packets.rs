//! Packet reassembly over the pages of one logical stream.
use crate::page::PageHeader;
use triergon_core::{BudgetVec, Bytes, Error, Result};

/// A packet completed by a page.
#[derive(Clone, Debug)]
pub struct Completed {
    /// The packet bytes: a view of the page for a packet inside it, or the gathered
    /// pieces of a packet that spans pages.
    pub payload: Bytes,
    /// Whether this is the last packet completed on its page, the one the page's granule
    /// position describes.
    pub last_on_page: bool,
}

/// What a page did to the packet in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuity {
    /// The page follows the previous one.
    Continuous,
    /// The first page after [`Assembler::reset`], whose leading continuation (if any)
    /// is skipped as RFC 3533 requires.
    Fresh,
    /// Bytes were lost: a sequence gap, a continuation with no packet in progress, or a
    /// packet that never finished. The broken packet is dropped.
    Lost,
}

/// Reassembles the packets of one logical stream from its pages, charging gathered bytes
/// to a budget. Pages must arrive in sequence; a gap drops the packet in progress.
///
/// Only a page's first piece can continue a packet and only its last piece can be
/// unfinished, so a page gathers at most one packet; every other packet is a view of
/// the page. [`Assembler::push`] either applies a whole page or fails without change.
#[derive(Debug)]
pub struct Assembler {
    /// The packet that continues onto the next page.
    partial: BudgetVec<u8>,
    next_sequence: Option<u32>,
    /// Bytes of the packet in progress were lost; its remainder is skipped.
    broken: bool,
    max_packet: usize,
}

impl Assembler {
    /// An assembler holding packets of at most `max_packet` bytes.
    pub fn new(budget: &triergon_core::Budget, max_packet: usize) -> Assembler {
        Assembler {
            partial: BudgetVec::new(budget),
            next_sequence: None,
            broken: false,
            max_packet,
        }
    }

    /// Forget the packet in progress and the expected sequence number, as after a seek.
    pub fn reset(&mut self) {
        self.partial.clear();
        self.next_sequence = None;
        self.broken = false;
    }

    /// Whether a packet continues past the last page pushed.
    pub fn in_progress(&self) -> bool {
        !self.partial.is_empty()
    }

    fn check(&self, length: usize) -> Result<()> {
        if length > self.max_packet {
            return Err(Error::Limit {
                requested: length,
                maximum: self.max_packet,
            });
        }
        Ok(())
    }

    /// Add one whole page (`page` starts with `header`) and append the packets it
    /// completes to `out`, which is cleared first.
    pub fn push(
        &mut self,
        header: &PageHeader,
        page: &Bytes,
        out: &mut Vec<Completed>,
    ) -> Result<Continuity> {
        out.clear();
        let body = header.header_len();
        if page.len() < header.page_len() {
            return Err(Error::Truncated);
        }
        let gap = self
            .next_sequence
            .is_some_and(|expected| header.sequence != expected);
        let fresh = self.next_sequence.is_none();
        // Whether the first piece continues the gathered packet, or is skipped as the
        // remainder of a packet whose start was lost.
        let continues = header.continued() && !gap && self.in_progress();
        let skips_first = header.continued() && !continues;
        let mut lost = gap || (!header.continued() && (self.in_progress() || self.broken));
        lost |= skips_first && !self.broken && !fresh;

        let count = header.pieces(page).count();
        let mut first_complete = false;
        let mut next_partial = None;
        for (index, (range, complete)) in header.pieces(page).enumerate() {
            if index == 0 {
                first_complete = complete;
            }
            if !complete && !(index == 0 && (continues || skips_first)) {
                // The last piece starts a packet that continues on the next page.
                debug_assert_eq!(index + 1, count);
                self.check(range.len())?;
                let mut gathered = BudgetVec::new(self.partial.budget());
                gathered.try_extend_from_slice(&page[body + range.start..body + range.end])?;
                next_partial = Some(gathered);
            }
        }
        let mut pieces = header.pieces(page);
        let mut rest_broken = false;
        if continues {
            let (range, complete) = pieces
                .next()
                .ok_or(Error::Invalid("Ogg page without pieces"))?;
            let length = self.partial.len() + range.len();
            self.check(length)?;
            let before = self.partial.len();
            self.partial.try_reserve_exact(range.len())?;
            self.partial
                .try_extend_from_slice(&page[body + range.start..body + range.end])?;
            if complete {
                let budget = self.partial.budget().clone();
                let gathered = std::mem::replace(&mut self.partial, BudgetVec::new(&budget));
                match gathered.freeze() {
                    Ok(payload) => out.push(Completed {
                        payload,
                        last_on_page: false,
                    }),
                    Err(rejected) => {
                        self.partial = rejected.buffer;
                        self.partial.truncate(before);
                        return Err(rejected.error);
                    }
                }
            }
        } else if skips_first {
            pieces.next();
            rest_broken = !first_complete;
        }
        // From here the page applies without failing.
        if !continues {
            self.partial.clear();
        }
        self.broken = rest_broken;
        self.next_sequence = Some(header.sequence.wrapping_add(1));
        for (range, complete) in pieces {
            if complete {
                out.push(Completed {
                    payload: page.slice(body + range.start..body + range.end)?,
                    last_on_page: false,
                });
            }
        }
        if let Some(gathered) = next_partial {
            self.partial = gathered;
        }
        if let Some(last) = out.last_mut() {
            last.last_on_page = true;
        }
        Ok(if lost {
            Continuity::Lost
        } else if fresh {
            Continuity::Fresh
        } else {
            Continuity::Continuous
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{CONTINUED, write_page};
    use triergon_core::Budget;

    fn push(assembler: &mut Assembler, page: &[u8], _: &Budget) -> (Continuity, Vec<Vec<u8>>) {
        let header = PageHeader::parse(page).unwrap();
        let bytes = Bytes::copy_from_slice(&Budget::new(1 << 20).unwrap(), page).unwrap();
        let mut out = Vec::new();
        let continuity = assembler.push(&header, &bytes, &mut out).unwrap();
        (continuity, out.iter().map(|c| c.payload.to_vec()).collect())
    }

    #[test]
    fn a_sequence_gap_drops_the_broken_packet() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut assembler = Assembler::new(&budget, 1 << 16);
        let mut page = Vec::new();
        write_page(0, -1, 1, 0, &[255], &[1; 255], &mut page);
        let (continuity, done) = push(&mut assembler, &page, &budget);
        assert_eq!((continuity, done.len()), (Continuity::Fresh, 0));
        assert!(assembler.in_progress());
        page.clear();
        write_page(CONTINUED, 9, 1, 2, &[10, 4], &[2; 14], &mut page);
        let (continuity, done) = push(&mut assembler, &page, &budget);
        assert_eq!(continuity, Continuity::Lost);
        assert_eq!(done, [vec![2; 4]]);
        assert!(!assembler.in_progress());
    }

    #[test]
    fn a_lost_middle_skips_until_the_packet_ends() {
        let budget = Budget::new(1 << 20).unwrap();
        let mut assembler = Assembler::new(&budget, 1 << 16);
        let mut page = Vec::new();
        // A fresh stream starting inside a packet: the continuation is skipped quietly.
        write_page(CONTINUED, -1, 1, 5, &[255], &[1; 255], &mut page);
        let (continuity, done) = push(&mut assembler, &page, &budget);
        assert_eq!((continuity, done.len()), (Continuity::Fresh, 0));
        page.clear();
        write_page(CONTINUED, 3, 1, 6, &[2, 3], &[7; 5], &mut page);
        let (continuity, done) = push(&mut assembler, &page, &budget);
        assert_eq!(continuity, Continuity::Continuous);
        assert_eq!(done, [vec![7; 3]]);
        // A page that starts fresh while a packet is in progress loses it.
        page.clear();
        write_page(0, -1, 1, 7, &[255], &[1; 255], &mut page);
        push(&mut assembler, &page, &budget);
        page.clear();
        write_page(0, 1, 1, 8, &[1], &[4], &mut page);
        let (continuity, done) = push(&mut assembler, &page, &budget);
        assert_eq!((continuity, done), (Continuity::Lost, vec![vec![4]]));
    }

    #[test]
    fn oversized_packets_and_budgets_fail_without_change() {
        let budget = Budget::new(Budget::control_storage_bytes() + 400).unwrap();
        let mut assembler = Assembler::new(&budget, 300);
        let mut page = Vec::new();
        write_page(0, -1, 1, 0, &[255], &[1; 255], &mut page);
        push(&mut assembler, &page, &budget);
        page.clear();
        write_page(CONTINUED, -1, 1, 1, &[255], &[1; 255], &mut page);
        let header = PageHeader::parse(&page).unwrap();
        let bytes = Bytes::copy_from_slice(&Budget::new(1 << 20).unwrap(), &page).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            assembler.push(&header, &bytes, &mut out),
            Err(Error::Limit {
                requested: 510,
                maximum: 300
            })
        ));
        assert!(assembler.in_progress());
    }
}
