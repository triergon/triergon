#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod blocking;
pub mod compat;
pub mod mapping;
mod packets;
pub mod page;
mod queue;
mod reader;
mod seek;
mod timing;
mod track;

pub use blocking::BlockingReader;
pub use mapping::Mapping;
pub use page::PageHeader;
pub use reader::{Limits, Reader, ReaderEvent};
pub use track::{Packet, Track};

use packets::Assembler;
use page::Examined;
use triergon_core::{Budget, BudgetVec, Bytes, Result};

/// One packet of a whole Ogg file read by [`read_packets`].
#[derive(Clone, Debug)]
pub struct SlicePacket {
    /// The serial number of the packet's logical stream.
    pub serial: u32,
    /// The packet bytes: a view of the file for a packet inside one page, gathered
    /// storage for one that spans pages.
    pub data: Bytes,
    /// The granule position of the page this packet ends on, when it is the last packet
    /// completed there; `None` otherwise.
    pub granule: Option<i64>,
    /// Whether this is the last packet of an end-of-stream page.
    pub end_of_stream: bool,
}

/// Every packet of every logical stream of a complete Ogg file in memory, in page order,
/// header packets included, charged to `pool`: one copy of the file, the packets that
/// span pages and the packet list. Bytes that are not a page, pages with a bad CRC and
/// a trailing partial page are skipped. For tests and tools; the reader is [`Reader`].
pub fn read_packets(pool: &Budget, bytes: &[u8]) -> Result<BudgetVec<SlicePacket>> {
    let file = Bytes::copy_from_slice(pool, bytes)?;
    let packet_bytes = Limits::default().packet_bytes;
    let mut streams: BudgetVec<(u32, Assembler)> = BudgetVec::new(pool);
    let mut packets = BudgetVec::new(pool);
    // At most one packet per lacing value of a page.
    let mut completed = Vec::with_capacity(255);
    let mut at = 0;
    while at < file.len() {
        let header = match page::examine(&file[at..]) {
            Examined::Page(header) => header,
            Examined::Garbage(skipped) => {
                at += skipped;
                continue;
            }
            Examined::Need(_) => break,
        };
        let page = file.slice(at..at + header.page_len())?;
        let found = streams
            .iter()
            .position(|(serial, _)| *serial == header.serial);
        let index = if let Some(index) = found {
            index
        } else {
            let stream = (header.serial, Assembler::new(pool, packet_bytes));
            streams.try_push(stream).map_err(|refused| refused.error)?;
            streams.len() - 1
        };
        streams[index].1.push(&header, &page, &mut completed)?;
        for packet in completed.drain(..) {
            let packet = SlicePacket {
                serial: header.serial,
                data: packet.payload,
                granule: packet.last_on_page.then_some(header.granule),
                end_of_stream: packet.last_on_page && header.end_of_stream(),
            };
            packets.try_push(packet).map_err(|refused| refused.error)?;
        }
        at += header.page_len();
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::page::{BEGIN_OF_STREAM, CONTINUED, END_OF_STREAM, write_page};
    use super::*;

    #[test]
    fn packets_span_pages_and_keep_their_granules() {
        let big: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut file = b"junk".to_vec();
        write_page(BEGIN_OF_STREAM, 0, 7, 0, &[3], b"abc", &mut file);
        // A 600-byte packet: 255 + 255 on this page, 90 on the next, then a 1-byte packet.
        write_page(0, -1, 7, 1, &[255, 255], &big[..510], &mut file);
        let mut body = big[510..].to_vec();
        body.push(b'z');
        write_page(
            CONTINUED | END_OF_STREAM,
            1234,
            7,
            2,
            &[90, 1],
            &body,
            &mut file,
        );
        let pool = Budget::new(1 << 20).unwrap();
        let packets = read_packets(&pool, &file).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(&packets[0].data[..], b"abc");
        assert_eq!(packets[0].granule, Some(0));
        assert_eq!(packets[1].data[..], big[..]);
        assert_eq!(packets[1].granule, None);
        assert_eq!(&packets[2].data[..], b"z");
        assert_eq!(packets[2].granule, Some(1234));
        assert!(packets[2].end_of_stream);
        let page = &file[4..];
        let header = PageHeader::parse(page).unwrap();
        assert!(header.crc_ok(page));
        file[34] ^= 1;
        assert!(!PageHeader::parse(&file[4..]).unwrap().crc_ok(&file[4..]));
        assert_eq!(read_packets(&pool, &file).unwrap().len(), 2);
    }
}
