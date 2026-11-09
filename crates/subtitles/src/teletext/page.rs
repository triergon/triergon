//! Page reception (EN 300 706 sections 7.2 and 9.3): assembling the packets of
//! each magazine into pages.

use super::coding::{hamming_24_18, odd_parity};
use super::packet::{Header, Packet, PageNumber, Subcode};
use super::presentation::Presentations;
use triergon_core::Result;

/// Display rows a page carries after its header: X/1 to X/25.
const ROWS: usize = 25;
/// Designations of the X/26 packets of one page.
const DESIGNATIONS: usize = 16;
/// Triplets of the X/26 packets of one page.
pub(super) const TRIPLETS: usize = DESIGNATIONS * 13;

/// The seven-bit codes of one display row.
pub(crate) type RowCodes = [u8; 40];
/// The 13 Hamming 24/18 triplets of an X/26 or X/28 packet, as transmitted.
pub(crate) type Triplets = [[u8; 3]; 13];

/// One page: its header and the packets received for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Page {
    pub header: Header,
    rows: [Option<RowCodes>; ROWS],
    enhancements: [Option<Triplets>; DESIGNATIONS],
    /// Whether an X/26 packet out of designation order ended their reception.
    enhancements_ended: bool,
    /// The X/28 format 1 packets.
    pub presentations: Presentations,
}

impl Page {
    fn new(header: Header) -> Self {
        Self {
            header,
            rows: [None; ROWS],
            enhancements: [None; DESIGNATIONS],
            enhancements_ended: false,
            presentations: Presentations::default(),
        }
    }

    /// The codes of display row `row` (1 to 25), if it was received.
    #[must_use]
    pub(crate) fn row(&self, row: u8) -> Option<&RowCodes> {
        self.rows.get(usize::from(row).checked_sub(1)?)?.as_ref()
    }

    /// Whether the X/26 packet with designation `designation` was received.
    #[must_use]
    pub(crate) fn has_enhancement(&self, designation: u8) -> bool {
        self.enhancements
            .get(usize::from(designation))
            .is_some_and(Option::is_some)
    }

    /// The X/26 packets in designation order, up to the first one missing.
    pub(crate) fn enhancements(&self) -> impl Iterator<Item = &Triplets> {
        self.enhancements.iter().map_while(Option::as_ref)
    }

    /// The triplets of the X/26 packets, the page's own enhancements and the
    /// objects they define, triplet `t` of designation `d` the `13 d + t`th.
    /// A triplet Hamming 24/18 cannot correct ends them: the active position
    /// after it is unknown.
    pub(crate) fn triplets(&self) -> impl Iterator<Item = Triplet> {
        self.enhancements()
            .flatten()
            .map_while(|triplet| hamming_24_18(*triplet).map(Triplet::new))
    }

    /// Receives a packet of the page; reports whether it was a usable X/28
    /// format 1 packet.
    fn receive(&mut self, packet: Packet<'_>) -> bool {
        match packet {
            Packet::Row { row, data } => {
                // A row with a parity error is not received; it also drops
                // an earlier copy of the row in the same transmission.
                let mut codes = [0; 40];
                let valid = data
                    .iter()
                    .zip(&mut codes)
                    .all(|(&byte, code)| odd_parity(byte).map(|c| *code = c).is_some());
                self.rows[usize::from(row - 1)] = valid.then_some(codes);
            }
            Packet::PageData {
                row,
                designation,
                data,
            } => {
                let triplets = triplets(data);
                match row {
                    // X/26 packets are sent in designation order from 0: the
                    // first that does not follow the last ends their reception.
                    26 if !self.enhancements_ended => {
                        let next = self.enhancements.iter().position(Option::is_none);
                        if next == Some(usize::from(designation)) {
                            self.enhancements[usize::from(designation)] = Some(triplets);
                        } else {
                            self.enhancements_ended = true;
                        }
                    }
                    28 => return self.presentations.receive(designation, &triplets),
                    _ => {}
                }
            }
            Packet::Header(_)
            | Packet::DamagedHeader
            | Packet::MagazineData { .. }
            | Packet::Other { .. } => {}
        }
        false
    }

    /// The page number and subcode.
    #[must_use]
    pub(crate) fn key(&self) -> (PageNumber, Subcode) {
        (self.header.page, self.header.subcode)
    }

    /// Takes every packet this page did not receive from `previous`, as a
    /// page sent without C4 (erase) keeps the content it does not repeat.
    pub(crate) fn complete_from(&mut self, previous: &Self) {
        fill(&mut self.rows, &previous.rows);
        fill(&mut self.enhancements, &previous.enhancements);
        self.presentations = self.presentations.over(&previous.presentations);
    }
}

fn triplets(data: &[u8; 39]) -> Triplets {
    let mut triplets = [[0; 3]; 13];
    for (triplet, bytes) in triplets.iter_mut().zip(data.chunks_exact(3)) {
        triplet.copy_from_slice(bytes);
    }
    triplets
}

fn fill<T: Copy, const N: usize>(slots: &mut [Option<T>; N], previous: &[Option<T>; N]) {
    for (slot, old) in slots.iter_mut().zip(previous) {
        if slot.is_none() {
            *slot = *old;
        }
    }
}

/// One X/26 triplet: an address, a mode of five bits and data of seven bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Triplet {
    /// Addresses 40 to 63: a row (address 40 is row 24) and a row mode.
    Row { row: u8, mode: u8, data: u8 },
    /// Addresses 0 to 39: a column and a column mode.
    Column { column: u8, mode: u8, data: u8 },
}

impl Triplet {
    fn new(value: u32) -> Self {
        let address = (value & 0x3F) as u8;
        let mode = (value >> 6 & 0x1F) as u8;
        let data = (value >> 11 & 0x7F) as u8;
        match address {
            0..40 => Self::Column {
                column: address,
                mode,
                data,
            },
            40 => Self::Row {
                row: 24,
                mode,
                data,
            },
            _ => Self::Row {
                row: address - 40,
                mode,
                data,
            },
        }
    }
}

/// What one packet brought.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Reception<'a> {
    /// A page the packet completed, with the packets of this transmission,
    /// and the M/29 packets its magazine had then.
    Page {
        page: &'a Page,
        magazine: &'a Presentations,
    },
    /// The packet's page header, after the pages it ends.
    Header(&'a Header),
    /// An X/28 format 1 packet the page being received took, with the M/29
    /// packets of its magazine.
    Presentation {
        page: &'a Page,
        magazine: &'a Presentations,
    },
}

/// Assembles the packets of all magazines into page transmissions.
///
/// A page ends when the next header of its magazine arrives with another page
/// number, or, in serial transmission (C11), when any header arrives. A header
/// with the same page number restarts the page; page `FF` only ends the
/// previous one; a header that cannot be corrected discards the page in
/// progress. A completed page holds only the packets of its transmission: a
/// receiver keeping page memory merges a page sent without C4 (erase) onto its
/// previous copy with [`Page::complete_from`].
#[derive(Clone, Debug, Default)]
pub(crate) struct Assembler {
    receiving: [Option<Page>; 8],
    magazines: [Presentations; 8],
}

impl Assembler {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Receives one packet. `observe` sees every page the packet ends, in
    /// magazine order, and then the header; its error stops the packet.
    pub(crate) fn push(
        &mut self,
        packet: &[u8; 42],
        mut observe: impl FnMut(Reception<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some((address, content)) = Packet::parse(packet) else {
            return Ok(());
        };
        let magazine = address.magazine.index();
        match content {
            Packet::Header(header) => {
                for (index, slot) in self.receiving.iter_mut().enumerate() {
                    let ends = |page: &Page| {
                        if index == magazine {
                            page.header.page != header.page
                        } else {
                            page.header.control.serial
                        }
                    };
                    if let Some(page) = slot.take_if(|page| ends(page)) {
                        let magazine = &self.magazines[index];
                        observe(Reception::Page {
                            page: &page,
                            magazine,
                        })?;
                    }
                }
                observe(Reception::Header(&header))?;
                self.receiving[magazine] =
                    (!header.page.is_time_filler()).then(|| Page::new(header));
            }
            Packet::DamagedHeader => self.receiving[magazine] = None,
            Packet::MagazineData { designation, data } => {
                self.magazines[magazine].receive(designation, &triplets(data));
            }
            other => {
                if let Some(page) = &mut self.receiving[magazine]
                    && page.receive(other)
                {
                    observe(Reception::Presentation {
                        page,
                        magazine: &self.magazines[magazine],
                    })?;
                }
            }
        }
        Ok(())
    }
}
