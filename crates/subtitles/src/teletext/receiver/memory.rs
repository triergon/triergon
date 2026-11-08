//! libzvbi's page memory: the last copy of each page and subpage, how a page
//! sent without C4 (erase) builds on it, and which M/29 packets a page is
//! presented with.

use crate::teletext::packet::{Header, Magazine, PageNumber, Subcode};
use crate::teletext::page::Page;
use crate::teletext::presentation::Presentations;
use triergon_core::{Budget, BudgetVec, Result};

/// The subcode of a page without subpages that matches any subcode.
const ANY_SUBCODE: Subcode = Subcode(0x3F7F);

/// Copies are kept per page and subcode, replacing the oldest when full; a
/// copy is as new as its last use, stored or built on. A page replaces the
/// copy it built on; else a page without subpages (subcode 0000 or 3F7F)
/// replaces the newest copy of its number. A page builds on the newest copy
/// of its subcode (for 0000 also 3F7F), or for 3F7F of any subcode.
///
/// A page sent without C4 keeps the packets of its copy that it does not
/// repeat, unless it is a subtitle page (C6 and C7, not C5) whose header
/// directly follows a header of the same page that did not keep its copy.
///
/// A transmission with X/28 packets is presented with them over the M/29
/// packets its magazine had at the first of them; one without keeps the
/// presentation of its copy that had them, else takes its magazine's M/29
/// packets when it completes.
///
/// Changes since the last [`commit`](Self::commit) can be undone with
/// [`rollback`](Self::rollback); a change that fails makes none.
#[derive(Debug)]
pub(super) struct PageMemory {
    copies: BudgetVec<Remembered>,
    capacity: usize,
    clock: u64,
    /// The page each magazine is receiving.
    transmissions: [Transmission; 8],
    /// The page of the last header, if it did not keep its copy.
    last_dropped: Option<PageNumber>,
    /// How to undo the changes to `copies` since the last commit, oldest first.
    undo: BudgetVec<Undo>,
    /// The copies those changes replaced, oldest first.
    replaced: BudgetVec<Remembered>,
}

/// One change to the copies, undone.
#[derive(Clone, Copy, Debug)]
enum Undo {
    /// The copy at this position was last used then.
    Used(usize, u64),
    /// The copy at this position was the newest in `replaced`.
    Replaced(usize),
    /// A copy was added after the others.
    Added,
}

/// The page memory's state besides its copies, to go back to.
#[derive(Clone, Copy, Debug)]
pub(super) struct Checkpoint {
    clock: u64,
    transmissions: [Transmission; 8],
    last_dropped: Option<PageNumber>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Transmission {
    /// Whether the page keeps its last copy.
    keeps_previous: bool,
    /// The magazine's M/29 packets at the page's first X/28 packet.
    magazine: Option<Presentations>,
}

#[derive(Debug)]
struct Remembered {
    page: Page,
    /// The presentation of a page with X/28 packets.
    presentations: Option<Presentations>,
    stored: u64,
}

/// A completed page with the packets of its last copy and the X/28 and M/29
/// packets it is presented with.
#[derive(Debug)]
pub(super) struct Completed {
    pub(super) page: Page,
    pub(super) presentations: Presentations,
}

impl PageMemory {
    pub(super) fn new(pool: &Budget, capacity: usize) -> Self {
        Self {
            copies: BudgetVec::new(pool),
            capacity,
            clock: 0,
            transmissions: [Transmission::default(); 8],
            last_dropped: None,
            undo: BudgetVec::new(pool),
            replaced: BudgetVec::new(pool),
        }
    }

    pub(super) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            clock: self.clock,
            transmissions: self.transmissions,
            last_dropped: self.last_dropped,
        }
    }

    /// Keeps the changes made since the last commit.
    pub(super) fn commit(&mut self) {
        self.undo.clear();
        self.replaced.clear();
    }

    /// Undoes the changes made since the last commit, back to `checkpoint`
    /// taken then.
    pub(super) fn rollback(&mut self, checkpoint: Checkpoint) {
        while let Some(undo) = self.undo.pop() {
            let copies = self.copies.as_mut_slice();
            match undo {
                Undo::Used(index, stored) => copies[index].stored = stored,
                Undo::Replaced(index) => {
                    if let Some(copy) = self.replaced.pop() {
                        copies[index] = copy;
                    }
                }
                Undo::Added => {
                    self.copies.pop();
                }
            }
        }
        self.clock = checkpoint.clock;
        self.transmissions = checkpoint.transmissions;
        self.last_dropped = checkpoint.last_dropped;
    }

    /// Records a change made by `change`, which runs only once there is room
    /// to record it and fails without making it.
    fn record(&mut self, change: impl FnOnce(&mut Self) -> Result<Undo>) -> Result<()> {
        reserve_one(&mut self.undo)?;
        let undo = change(self)?;
        self.undo.try_push(undo).map_err(|rejected| rejected.error)
    }

    /// Starts the reception of the page `header` introduces. A page that
    /// keeps its last copy uses it from its header, even if it does not
    /// complete.
    pub(super) fn begin(&mut self, header: &Header) -> Result<()> {
        let control = header.control;
        let follows_dropped = self.last_dropped == Some(header.page);
        let subtitle = control.subtitle && control.suppress_header && !control.newsflash;
        let keeps_previous = !(control.erase || follows_dropped && subtitle);
        if keeps_previous && let Some(index) = self.find((header.page, header.subcode)) {
            self.record(|memory| {
                memory.clock += 1;
                let copy = &mut memory.copies.as_mut_slice()[index];
                Ok(Undo::Used(
                    index,
                    std::mem::replace(&mut copy.stored, memory.clock),
                ))
            })?;
        }
        self.transmissions[header.page.magazine.index()] = Transmission {
            keeps_previous,
            magazine: None,
        };
        self.last_dropped = (!keeps_previous).then_some(header.page);
        Ok(())
    }

    /// Notes that the page `magazine` is receiving took an X/28 packet while
    /// the magazine had the M/29 packets `packets`.
    pub(super) fn presentation_received(&mut self, magazine: Magazine, packets: &Presentations) {
        let transmission = &mut self.transmissions[magazine.index()];
        transmission.magazine.get_or_insert(*packets);
    }

    /// Completes `page` from its copy where it keeps it and stores it;
    /// `magazine` holds its magazine's M/29 packets.
    pub(super) fn complete(
        &mut self,
        mut page: Page,
        magazine: &Presentations,
    ) -> Result<Completed> {
        let transmission = self.transmissions[page.header.page.magazine.index()];
        let mut presentations = transmission.magazine.map(|m| page.presentations.over(&m));
        let previous = transmission
            .keeps_previous
            .then(|| self.find(page.key()))
            .flatten();
        if let Some(index) = previous {
            let copy = &self.copies.as_slice()[index];
            page.complete_from(&copy.page);
            presentations = presentations.or(copy.presentations);
        }
        let (number, subcode) = page.key();
        self.clock += 1;
        let entry = Remembered {
            page: page.clone(),
            presentations,
            stored: self.clock,
        };
        // The copy it built on, else for a page without subpages the newest
        // copy of its number, else the copy of its subcode.
        let replaced = previous.or_else(|| {
            if subcode == Subcode(0) || subcode == ANY_SUBCODE {
                self.newest(number, |_| true)
            } else {
                self.newest(number, |stored| stored == subcode)
            }
        });
        let replaced = replaced.or_else(|| {
            let copies = self.copies.as_slice();
            (copies.len() >= self.capacity)
                .then(|| (0..copies.len()).min_by_key(|&i| copies[i].stored))
                .flatten()
        });
        self.record(|memory| {
            if let Some(index) = replaced {
                reserve_one(&mut memory.replaced)?;
                let copy = &mut memory.copies.as_mut_slice()[index];
                let previous = std::mem::replace(copy, entry);
                memory
                    .replaced
                    .try_push(previous)
                    .map_err(|rejected| rejected.error)?;
                Ok(Undo::Replaced(index))
            } else {
                memory.copies.try_push(entry).map_err(|error| error.error)?;
                Ok(Undo::Added)
            }
        })?;
        Ok(Completed {
            page,
            presentations: presentations.unwrap_or(*magazine),
        })
    }

    /// The position of the newest copy of `number` whose subcode `matches`.
    fn newest(&self, number: PageNumber, matches: impl Fn(Subcode) -> bool) -> Option<usize> {
        let copies = self.copies.as_slice();
        (0..copies.len())
            .filter(|&i| {
                copies[i].page.header.page == number && matches(copies[i].page.header.subcode)
            })
            .max_by_key(|&i| copies[i].stored)
    }

    /// The copy a page sent without C4 builds on.
    fn find(&self, (number, subcode): (PageNumber, Subcode)) -> Option<usize> {
        self.newest(number, |stored| {
            subcode == ANY_SUBCODE
                || stored == subcode
                || (subcode == Subcode(0) && stored == ANY_SUBCODE)
        })
    }
}

/// Room for one more element, growing geometrically.
fn reserve_one<T>(vec: &mut BudgetVec<T>) -> Result<()> {
    if vec.len() == vec.capacity() {
        vec.try_reserve_exact(vec.len().max(4))?;
    }
    Ok(())
}
