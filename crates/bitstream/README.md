# triergon-bitstream

Bit-level primitives shared by the format, framing and codec crates. It sits in the
foundation layer, depends only on `std`, and knows nothing about containers, codecs or
any crate above it. Errors a parser reports stay in that parser: everything here returns
plain values, so a caller chooses its own error type and text.

## Decode loops

[`BitReader`] and [`CachedBitReader`] read MSB-first bits of a slice and yield zeros past
what they can reach, so a decoder polls [`Reader::overrun`] at syntax boundaries instead
of branching on every field. They share every method and differ in how they reach the
bits, which can decide several percent of a decoder's time:

- `BitReader` does one unaligned eight-byte load per read and keeps no state. Use it for
  fixed-width fields and short tables, as AC-3 and H.264 do. A load that does not fit in
  the slice reads as zero, so the slice carries eight bytes of padding after its last
  field, and [`Reader::with_end`] says where the fields end.
- `CachedBitReader` keeps eight bytes in a register, so a read is a shift and consecutive
  codewords do not wait for a load. Use it for Huffman codes, as AAC does. It reads every
  byte of the slice and needs no padding.

```rust
use triergon_bitstream::CachedBitReader;

let mut reader = CachedBitReader::new(&[0b1010_1100, 0xff]);
assert_eq!(reader.read(4), 0b1010);
assert_eq!(reader.read_signed(4), -4);
assert_eq!(reader.peek32(), 0xff00_0000);
reader.skip(16);
assert_eq!(reader.read(8), 0);
assert!(reader.overrun());
```

## Checked reads

[`BitCursor`] reads MSB-first bits of a slice and fails at its end. Headers and parameter
sets use it, or [`bits_at`] where a parser keeps its own position. The [`BitSource`] trait
adds Exp-Golomb codes over any such source.

```rust
use triergon_bitstream::{BitCursor, BitError, BitSource};

// 1 | 010 | 011 | 00100 | 00101
let mut bits = BitCursor::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
assert_eq!(bits.ue(), Ok(0));
assert_eq!(bits.read(3), Ok(0b010));
assert_eq!(bits.ue(), Ok(2));
assert_eq!(bits.ue(), Ok(3));
assert_eq!(bits.se(), Ok(-2));
assert_eq!(bits.read(8), Err(BitError::End));
```

[`Rbsp`] is the same trait over a NAL unit payload: it removes emulation prevention bytes
as it reads, so parsing a prefix copies nothing. [`unescape`] copies a whole payload for a
decoder.

```rust
use triergon_bitstream::{BitSource, Rbsp};

let mut rbsp = Rbsp::new(&[0, 0, 3, 1]);
assert_eq!(rbsp.read(24), Ok(1));
```

## Checksums

[`crc`] holds the three checksums the workspace uses. Each function continues from a
register value, so a message can arrive in pieces.

```rust
use triergon_bitstream::crc::{Crc32Ieee, Crc32Mpeg, crc16_a52};

// ATSC A/52 frames: a message followed by its CRC leaves a zero register.
let mut frame = b"payload".to_vec();
let crc = crc16_a52(0, &frame);
frame.extend_from_slice(&crc.to_be_bytes());
assert_eq!(crc16_a52(0, &frame), 0);

// MPEG-2 PSI sections, whole or byte by byte.
assert_eq!(Crc32Mpeg::of(b"123456789"), 0x0376_e6e7);
let mut section = Crc32Mpeg::default();
b"123456789".iter().for_each(|&byte| section.update_byte(byte));
assert_eq!(section.remainder(), 0x0376_e6e7);

// EBML CRC-32 elements.
assert_eq!(Crc32Ieee::of(b"123456789"), 0xcbf4_3926);
```
