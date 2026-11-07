# triergon-bitstream

Bit-level primitives shared by the format, framing and codec crates. It sits in the
foundation layer, depends only on `std`, and knows nothing about containers, codecs or
any crate above it. Errors a parser reports stay in that parser: everything here returns
plain values, so a caller chooses its own error type and text.

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
as it reads, so parsing a prefix copies nothing.

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
