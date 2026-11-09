//! DVB teletext (ETSI EN 300 472, EN 300 706): page decoding and subtitle pages.
//!
//! The decoder is layered as the standards are:
//!
//! - [`DataField`] splits an EN 300 472 PES data field into data units and
//!   [`TeletextUnit`] takes one EBU teletext packet out of a unit.
//! - [`Packet`] decodes a packet's address and, for page headers, the page
//!   number, subcode and control bits (Hamming 8/4, [`coding`]).
//! - [`Assembler`] collects each magazine's packets into [`Page`]s, ending a
//!   page on the next header as parallel and serial transmission define, and
//!   keeps the last copy of each page for pages sent without C4 (erase).
//! - [`Presentation`] decodes the X/28 and M/29 format 1 packets: character
//!   set designations, colour lookup tables and their remapping.
//! - [`Display`] presents a page: Level 1 spacing attributes (colours, boxes,
//!   double size, mosaics, hold) and the X/26 enhancements (characters,
//!   colours, display attributes and the page's own active, adaptive and
//!   passive objects), with the [`CharacterSet`] a designation selects.
//!
//! FFmpeg's `libzvbi_teletextdec` decoder, which turns pages into text or ASS
//! subtitles, is [`compat::teletext`](crate::compat::teletext).

pub mod charset;
pub mod coding;
mod data_unit;
mod display;
mod packet;
mod page;
mod presentation;
pub(crate) mod receiver;

pub use charset::{CharacterSet, G0, G2, NationalSubset};
pub use data_unit::{
    DATA_UNIT_BYTES, DataField, DataUnitHeader, DataUnitKind, TeletextUnit,
    is_teletext_data_identifier,
};
pub use display::{COLUMNS, Cell, Charsets, Colour, Display, Glyph, Part, ROWS, Size, Style};
pub use packet::{Address, Control, Header, Magazine, Packet, PageNumber, Subcode};
pub use page::{Assembler, Page, Reception, RowCodes, Triplet, Triplets};
pub use presentation::{Designations, Presentation, Presentations, Remapping, Rgb};
