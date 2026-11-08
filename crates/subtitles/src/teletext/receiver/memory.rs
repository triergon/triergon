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
#[derive(Debug)]
pub(super) struct PageMemory {
    copies: BudgetVec<Remembered>,
    capacity: usize,
    clock: u64,
    /// The page each magazine is receiving.
    transmissions: [Transmission; 8],
    /// The page of the last header, if it did not keep its copy.
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
        }
    }

    /// Starts the reception of the page `header` introduces. A page that
    /// keeps its last copy uses it from its header, even if it does not
    /// complete.
    pub(super) fn begin(&mut self, header: &Header) {
        let control = header.control;
        let follows_dropped = self.last_dropped == Some(header.page);
        let subtitle = control.subtitle && control.suppress_header && !control.newsflash;
        let keeps_previous = !(control.erase || follows_dropped && subtitle);
        if keeps_previous && let Some(index) = self.find((header.page, header.subcode)) {
            self.clock += 1;
            self.copies.as_mut_slice()[index].stored = self.clock;
        }
        self.transmissions[header.page.magazine.index()] = Transmission {
            keeps_previous,
            magazine: None,
        };
        self.last_dropped = (!keeps_previous).then_some(header.page);
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
        let copies = self.copies.as_mut_slice();
        if let Some(index) = replaced {
            copies[index] = entry;
        } else if copies.len() < self.capacity {
            self.copies.try_push(entry).map_err(|error| error.error)?;
        } else if let Some(oldest) = copies.iter_mut().min_by_key(|r| r.stored) {
            *oldest = entry;
        }
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
