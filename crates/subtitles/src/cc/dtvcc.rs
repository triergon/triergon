//! DTVCC (CEA-708) caption channel packets and their service blocks.
//!
//! A packet starts with a `cc_type` 3 construct whose first byte is the packet
//! header: `sequence_number` (2 bits) and `packet_size_code` (6 bits, the packet
//! length in byte pairs including the header, 0 meaning 64 pairs). `cc_type` 2
//! constructs continue it. Its payload is a run of service blocks, each a
//! header (`service_number`, 3 bits, and `block_size`, 5 bits; service 7 takes
//! its number from an extended byte) followed by `block_size` bytes; a zero
//! header ends the run.

use super::cc_data::{CcType, Triplet};

/// The longest packet: 64 byte pairs.
const MAX_PACKET: usize = 128;

/// Gathers DTVCC packets from valid `cc_type` 2 and 3 constructs.
#[derive(Clone, Debug)]
pub struct Assembler {
    data: [u8; MAX_PACKET],
    len: usize,
    expected: usize,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data: [0; MAX_PACKET],
            len: 0,
            expected: 0,
        }
    }

    /// Takes one construct. Returns the packet it completes. A packet start
    /// abandons an incomplete packet; data outside a packet, invalid constructs
    /// and CEA-608 pairs are ignored.
    pub fn push(&mut self, triplet: Triplet) -> Option<Packet<'_>> {
        if !triplet.valid() {
            return None;
        }
        match triplet.kind() {
            CcType::DtvccStart => {
                let code = usize::from(triplet.data[0] & 0x3f);
                self.expected = if code == 0 { MAX_PACKET } else { code * 2 };
                self.data[..2].copy_from_slice(&triplet.data);
                self.len = 2;
            }
            CcType::DtvccData if self.len > 0 && self.len < self.expected => {
                self.data[self.len..self.len + 2].copy_from_slice(&triplet.data);
                self.len += 2;
            }
            _ => return None,
        }
        if self.len < self.expected {
            return None;
        }
        self.len = 0;
        Some(Packet {
            bytes: &self.data[..self.expected],
        })
    }
}

/// One complete DTVCC packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    bytes: &'a [u8],
}

impl<'a> Packet<'a> {
    /// `sequence_number`, 0 to 3.
    #[must_use]
    pub fn sequence(&self) -> u8 {
        self.bytes[0] >> 6
    }

    /// The packet after its header.
    #[must_use]
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[1..]
    }

    /// The service blocks, in order. A block whose header promises more bytes
    /// than the packet holds ends the run.
    #[must_use]
    pub fn service_blocks(&self) -> ServiceBlocks<'a> {
        ServiceBlocks {
            rest: self.payload(),
        }
    }
}

/// One service block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceBlock<'a> {
    /// `service_number`, 1 to 63.
    pub service: u8,
    pub data: &'a [u8],
}

/// Iterator over a packet's service blocks.
#[derive(Clone, Debug)]
pub struct ServiceBlocks<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for ServiceBlocks<'a> {
    type Item = ServiceBlock<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let (&header, mut rest) = self.rest.split_first()?;
        let size = usize::from(header & 0x1f);
        let mut service = header >> 5;
        if service == 0 {
            self.rest = &[];
            return None;
        }
        if service == 7 {
            let (&extended, after) = rest.split_first()?;
            service = extended & 0x3f;
            rest = after;
        }
        if size > rest.len() {
            self.rest = &[];
            return None;
        }
        let (data, after) = rest.split_at(size);
        self.rest = after;
        Some(ServiceBlock { service, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triplet(flags: u8, a: u8, b: u8) -> Triplet {
        Triplet {
            flags,
            data: [a, b],
        }
    }

    #[test]
    fn assembles_packets_and_service_blocks() {
        let mut assembler = Assembler::new();
        // Sequence 1, three pairs: header, service 1 with three bytes, then padding.
        assert!(assembler.push(triplet(0xff, 0x43, 0x23)).is_none());
        assert!(assembler.push(triplet(0xfe, b'H', b'i')).is_none());
        let packet = assembler.push(triplet(0xfe, b'!', 0)).expect("complete");
        assert_eq!(packet.sequence(), 1);
        let blocks: Vec<_> = packet.service_blocks().collect();
        assert_eq!(
            blocks,
            [ServiceBlock {
                service: 1,
                data: b"Hi!"
            }]
        );
        // Data outside a packet and invalid constructs are ignored.
        assert!(assembler.push(triplet(0xfe, 1, 2)).is_none());
        assert!(assembler.push(triplet(0xfb, 0x41, 0x00)).is_none());
    }
}
