#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Pending: `missing_docs`.

mod blocking;
pub mod compat;
pub mod mapping;
pub mod packets;
pub mod page;
mod queue;
mod reader;
pub mod seek;
mod timing;
mod track;

pub use blocking::BlockingReader;
pub use mapping::Mapping;
pub use packets::{Assembler, Completed, Continuity};
pub use page::PageHeader;
pub use reader::{Limits, Reader, ReaderEvent};
pub use track::{Packet, Track};

use triergon_core::{Budget, Bytes, Error, Result};

/// One packet of a whole Ogg file read by [`read_packets`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlicePacket {
    pub serial: u32,
    pub data: Vec<u8>,
    /// The granule position of the page this packet ends on, when it is the last packet
    /// completed there; `None` otherwise.
    pub granule: Option<i64>,
    pub end_of_stream: bool,
}

/// Every packet of every logical stream of a complete Ogg file in memory, in page order,
/// header packets included. Pages with a bad CRC are skipped. For tests and tools; the
/// reader is [`Reader`].
pub fn read_packets(bytes: &[u8]) -> Result<Vec<SlicePacket>> {
    let budget = Budget::new(bytes.len().saturating_mul(4).max(1 << 20))?;
    let mut streams: Vec<(u32, Assembler)> = Vec::new();
    let mut out = Vec::new();
    let mut done = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let header = match PageHeader::parse(&bytes[at..]) {
            Ok(header) => header,
            Err(Error::Truncated) => break,
            Err(_) => {
                at += 1;
                continue;
            }
        };
        let page = &bytes[at..];
        if page.len() < header.page_len() {
            break;
        }
        if !header.crc_ok(page) {
            at += 1;
            continue;
        }
        let found = streams
            .iter()
            .position(|(serial, _)| *serial == header.serial);
        let index = found.unwrap_or_else(|| {
            streams.push((header.serial, Assembler::new(&budget, 1 << 24)));
            streams.len() - 1
        });
        let shared = Bytes::copy_from_slice(&budget, &page[..header.page_len()])?;
        streams[index].1.push(&header, &shared, &mut done)?;
        for completed in &done {
            out.push(SlicePacket {
                serial: header.serial,
                data: completed.payload.to_vec(),
                granule: completed.last_on_page.then_some(header.granule),
                end_of_stream: completed.last_on_page && header.end_of_stream(),
            });
        }
        at += header.page_len();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::page::{BEGIN_OF_STREAM, CONTINUED, END_OF_STREAM, write_page};
    use super::*;

    #[test]
    fn packets_span_pages_and_keep_their_granules() {
        let big: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let mut file = Vec::new();
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
        let packets = read_packets(&file).unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].data, b"abc");
        assert_eq!(packets[0].granule, Some(0));
        assert_eq!(packets[1].data, big);
        assert_eq!(packets[1].granule, None);
        assert_eq!(packets[2].data, b"z");
        assert_eq!(packets[2].granule, Some(1234));
        assert!(packets[2].end_of_stream);
        let header = PageHeader::parse(&file).unwrap();
        assert!(header.crc_ok(&file));
        file[30] ^= 1;
        assert!(!PageHeader::parse(&file).unwrap().crc_ok(&file));
    }
}
