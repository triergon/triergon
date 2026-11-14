//! EN 300 706 packets and EN 300 472 PES payloads for the teletext tests.
#![allow(dead_code)]

const HAMMING_8_4: [u8; 16] = [
    0x15, 0x02, 0x49, 0x5E, 0x64, 0x73, 0x38, 0x2F, 0xD0, 0xC7, 0x8C, 0x9B, 0xA1, 0xB6, 0xFD, 0xEA,
];

pub fn ham(nibble: u8) -> u8 {
    HAMMING_8_4[usize::from(nibble & 15)]
}

pub fn parity(code: u8) -> u8 {
    let code = code & 0x7F;
    if code.count_ones() % 2 == 1 {
        code
    } else {
        code | 0x80
    }
}

fn address(magazine: u8, row: u8) -> [u8; 2] {
    [ham(magazine & 7 | (row & 1) << 3), ham(row >> 1)]
}

/// Page header flags.
#[derive(Clone, Copy, Default)]
pub struct Flags {
    pub erase: bool,
    pub newsflash: bool,
    pub subtitle: bool,
    pub suppress: bool,
    pub inhibit: bool,
    pub serial: bool,
    pub national: u8,
    pub subcode: u16,
}

/// C4, C6 and C7, as subtitle pages are sent.
pub const SUBTITLE: Flags = Flags {
    erase: true,
    newsflash: false,
    subtitle: true,
    suppress: true,
    inhibit: false,
    serial: false,
    national: 0,
    subcode: 0,
};

pub const NORMAL: Flags = Flags {
    erase: true,
    newsflash: false,
    subtitle: false,
    suppress: false,
    inhibit: false,
    serial: false,
    national: 0,
    subcode: 0,
};

pub fn header(magazine: u8, page: u8, flags: Flags) -> [u8; 42] {
    let s = flags.subcode;
    let mut packet = [0x80 | b' '; 42];
    packet[..2].copy_from_slice(&address(magazine, 0));
    let n = flags.national;
    let nibbles = [
        page & 0xF,
        page >> 4,
        (s & 0xF) as u8,
        (s >> 4 & 7) as u8 | u8::from(flags.erase) << 3,
        (s >> 8 & 0xF) as u8,
        (s >> 12 & 3) as u8 | u8::from(flags.newsflash) << 2 | u8::from(flags.subtitle) << 3,
        u8::from(flags.suppress) | u8::from(flags.inhibit) << 3,
        u8::from(flags.serial) | (n & 1) << 3 | (n >> 1 & 1) << 2 | (n >> 2 & 1) << 1,
    ];
    for (byte, nibble) in packet[2..10].iter_mut().zip(nibbles) {
        *byte = ham(nibble);
    }
    packet
}

pub fn row(magazine: u8, row: u8, text: &[u8]) -> [u8; 42] {
    let mut packet = [parity(b' '); 42];
    packet[..2].copy_from_slice(&address(magazine, row));
    for (byte, &code) in packet[2..].iter_mut().zip(text) {
        *byte = parity(code);
    }
    packet
}

/// An 18-bit value as a Hamming 24/18 triplet.
pub fn triplet(address: u8, mode: u8, data: u8) -> [u8; 3] {
    let value =
        u32::from(address & 0x3F) | u32::from(mode & 0x1F) << 6 | u32::from(data & 0x7F) << 11;
    let data_positions = [
        3, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23,
    ];
    let mut word = 0u32;
    for (i, position) in data_positions.iter().enumerate() {
        word |= (value >> i & 1) << (position - 1);
    }
    for k in 0..5 {
        let covered: u32 = (1..=23u32)
            .filter(|p| p & (1 << k) != 0 && *p != 1 << k)
            .map(|p| word >> (p - 1) & 1)
            .sum();
        word |= ((covered + 1) & 1) << ((1 << k) - 1);
    }
    word |= ((word.count_ones() + 1) & 1) << 23;
    [word as u8, (word >> 8) as u8, (word >> 16) as u8]
}

/// An X/26/0 packet of up to 13 triplets, padded with termination markers.
pub fn enhancement(magazine: u8, triplets: &[[u8; 3]]) -> [u8; 42] {
    designated_enhancement(magazine, 0, triplets)
}

/// An X/26 packet with designation `designation`.
pub fn designated_enhancement(magazine: u8, designation: u8, triplets: &[[u8; 3]]) -> [u8; 42] {
    let mut packet = [0; 42];
    packet[..2].copy_from_slice(&address(magazine, 26));
    packet[2] = ham(designation);
    for (i, slot) in packet[3..].chunks_exact_mut(3).enumerate() {
        slot.copy_from_slice(&triplets.get(i).copied().unwrap_or(triplet(0x3F, 0x1F, 0)));
    }
    packet
}

/// The PES payload of `packets`: data identifier 0x10 and one subtitle data
/// unit per packet, stuffed to fill whole transport packets.
pub fn payload(packets: &[[u8; 42]]) -> Vec<u8> {
    let mut out = vec![0x10];
    for (i, packet) in packets.iter().enumerate() {
        out.extend([0x03, 0x2C, 0xE0 | (7 + i as u8 % 16), 0xE4]);
        out.extend(packet.iter().map(|b| b.reverse_bits()));
    }
    while (out.len() + 45) % 184 != 0 {
        out.extend([0xFF, 0x2C]);
        out.extend([0xFF; 44]);
    }
    out
}
